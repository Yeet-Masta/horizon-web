//! A Java façade skeleton for the `net.minecraft` classes a mod jar names,
//! generated from the real 26.3 jar's shapes and compiled with `javac`
//! (`docs/java-mod-compat.md`, phase 1's first slice).
//!
//! The stubs are shapes, not behaviour: a method throws
//! `UnsupportedOperationException` naming itself, so a mod run against them
//! stops at the first thing the game must actually do. That is the probe: the
//! class loads, the entrypoint runs, and the failure names the next method to
//! implement.
//!
//! Nested classes (`HolderSet$Named`) are emitted nested, because `javac`
//! resolves `A.B` and not the flat `A$B`; references are written the Java way.
use crate::classes::{self, Class, Declared, MemberKind};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};

/// One generated shim member that stays object-shaped because the real
/// descriptor names a type this compilation cannot provide (a legacy name the
/// 26.3 game jar replaced, a loader type absent from the target's classpath, or
/// a game-free slice that must not name a game class). A caller can surface
/// the row instead of discovering the bound as a `NoSuchMethodError` at load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyBound {
    /// The shim class, as a binary name (`net/minecraftforge/...`).
    pub class: String,
    /// The kept member (`newSimpleChannel`, `playToServer/...`).
    pub member: String,
    /// Why the parameters cannot carry the real descriptor.
    pub reason: String,
}

/// Enough rows for every surface one compilation can emit; the cap keeps a
/// pathological target from turning the report into an unbounded log.
const MAX_LEGACY_BOUNDS: usize = 64;

#[derive(Debug, Default, Clone)]
pub struct Report {
    pub classes: usize,
    pub methods: usize,
    pub fields: usize,
    /// Named by the mod but not in the game jar (a library, or a version
    /// mismatch), and nested classes deeper than one level.
    pub missing: Vec<String>,
    /// Shim members emitted with `Object` parameters where the real API is
    /// typed. Deduplicated in emission order and capped at
    /// `MAX_LEGACY_BOUNDS`; empty when every emitted signature carries its real
    /// descriptor.
    pub legacy: Vec<LegacyBound>,
    pub sources: PathBuf,
    pub jar: PathBuf,
}

impl Report {
    /// Records one bounded legacy shim. Repeated emission of the same member
    /// (a mixed target naming a family twice) yields one row.
    fn bound_legacy(
        &mut self,
        class: impl Into<String>,
        member: impl Into<String>,
        reason: impl Into<String>,
    ) {
        if self.legacy.len() >= MAX_LEGACY_BOUNDS {
            return;
        }
        let row = LegacyBound {
            class: class.into(),
            member: member.into(),
            reason: reason.into(),
        };
        if !self.legacy.contains(&row) {
            self.legacy.push(row);
        }
    }
}

pub fn generate(
    mod_jar: &Path,
    game_jar: &Path,
    library_jars: &[PathBuf],
    out: &Path,
    javac: &Path,
    jar: &Path,
) -> Result<Report, String> {
    generate_many(
        &[mod_jar.to_path_buf()],
        game_jar,
        library_jars,
        out,
        javac,
        jar,
    )
}

/// Generate one façade from a mod and the loader/API jars it depends on.
/// Dependency jars contribute references and member uses, while their own
/// classes remain real entries on `javac`'s classpath.
pub fn generate_many(
    mod_jars: &[PathBuf],
    game_jar: &Path,
    library_jars: &[PathBuf],
    out: &Path,
    javac: &Path,
    jar: &Path,
) -> Result<Report, String> {
    let game: BTreeMap<String, Class> = classes::read_jar(game_jar)?
        .into_iter()
        .map(|c| (c.name.clone(), c))
        .collect();
    // Libraries are real, not stubbed: they go on `javac`'s and the game's
    // classpath. Only the game's own classes need a façade.
    let mut lib_names: BTreeSet<String> = BTreeSet::new();
    for path in library_jars {
        if let Ok(classes) = classes::read_jar(path) {
            lib_names.extend(classes.into_iter().map(|c| c.name));
        }
    }
    let mut target = Vec::new();
    for mod_jar in mod_jars {
        target.extend(classes::read_jar(mod_jar)?);
    }
    let uses_neoforge = target
        .iter()
        .any(|class| class.refs.iter().any(|r| r.starts_with("net/neoforged/")));
    // Legacy Forge jars do not carry NeoForge's package names, but they still
    // need the bounded loader/event/capability shims below. Keep this probe
    // separate so a Forge-only mod receives the same ABI coverage without
    // treating every game class as a loader dependency.
    let uses_legacy_forge = target.iter().any(|class| {
        class
            .refs
            .iter()
            .any(|r| r.starts_with("net/minecraftforge/"))
    });
    // Fabric's payload registry lives in the Fabric API jar, not in the game
    // jar: a target that registers payloads only links when that API is on the
    // compile classpath, so keep a bounded marker for the jar-only slice.
    let uses_fabric_payloads = target.iter().any(|class| {
        class
            .refs
            .iter()
            .any(|r| r == "net/fabricmc/fabric/api/networking/v1/PayloadTypeRegistry")
    });
    // Fabric's play-stage networking classes are library classes too. They only
    // appear in the façade when the real API jar is absent, and they retain the
    // handlers a mod registers instead of moving a payload anywhere.
    let names_fabric = |name: &str| {
        target
            .iter()
            .any(|class| class.refs.iter().any(|r| r.split('$').next() == Some(name)))
    };
    let uses_server_networking =
        names_fabric("net/fabricmc/fabric/api/networking/v1/ServerPlayNetworking");
    let uses_client_networking =
        names_fabric("net/fabricmc/fabric/api/client/networking/v1/ClientPlayNetworking");
    let uses_fabric_networking = uses_server_networking
        || uses_client_networking
        || names_fabric("net/fabricmc/fabric/api/networking/v1/PacketSender");
    // Fabric's data-attachment registry and NeoForge's attachment type are
    // library classes too. They are emitted only when the real API class is
    // absent, and they retain what a mod registers (id, codec/initializer
    // markers, flags) instead of attaching anything to a live target.
    let uses_fabric_attachments =
        names_fabric("net/fabricmc/fabric/api/attachment/v1/AttachmentRegistry")
            || names_fabric("net/fabricmc/fabric/api/attachment/v1/AttachmentType")
            || names_fabric("net/fabricmc/fabric/api/attachment/v1/AttachmentTarget");
    let uses_neoforge_attachments = uses_neoforge
        && target.iter().any(|class| {
            class
                .refs
                .iter()
                .any(|r| r.split('$').next() == Some(NEOFORGE_ATTACHMENT_TYPE))
        });
    // Menu and screen-handler registration is a library slice on both loaders.
    // The classes a jar references select it, so a target that never registers
    // a menu pays nothing; the emission guards below keep a real API jar from
    // being shadowed.
    let names_any = |names: &[&str]| {
        target.iter().any(|class| {
            class
                .refs
                .iter()
                .any(|r| names.contains(&r.split('$').next().unwrap_or(r)))
        })
    };
    let uses_fabric_menus = names_any(&[
        FABRIC_MENU_TYPE,
        FABRIC_MENU_FACTORY,
        FABRIC_SCREEN_HANDLER_TYPE,
        FABRIC_SCREEN_HANDLER_FACTORY,
    ]);
    let uses_neoforge_menus = uses_neoforge
        && names_any(&[
            NEOFORGE_CONTAINER_FACTORY,
            NEOFORGE_EXTENDED_CONTAINER_FACTORY,
            NEOFORGE_MENU_TYPE_EXTENSION,
        ]);
    // `ClientPlayNetworking.Context#player()` names the client player type. It
    // is a client-only game class (26.3 still calls it `LocalPlayer`), so a
    // server-shaped jar leaves it unresolved and the client shim is reported
    // instead of emitted with an invented type.
    let fabric_client_player = uses_client_networking
        .then(|| {
            [
                "net/minecraft/client/player/LocalPlayer",
                "net/minecraft/client/player/AbstractClientPlayer",
            ]
            .into_iter()
            .find(|name| game.contains_key(*name) && !lib_names.contains(*name))
        })
        .flatten();
    // Fabric's client modules (HUD rendering, client ticks, key bindings, item
    // tooltips) are library classes as well: they are emitted below only when
    // the real API class is absent, and only when the client game types their
    // callback signatures name resolve on this game jar.
    let fabric_client_holders = [
        "net/fabricmc/fabric/api/client/rendering/v1/HudRenderCallback",
        "net/fabricmc/fabric/api/client/event/lifecycle/v1/ClientTickEvents",
        "net/fabricmc/fabric/api/client/keybinding/v1/KeyBindingHelper",
        "net/fabricmc/fabric/api/client/item/tooltip/v1/ItemTooltipCallback",
    ];
    let uses_fabric_client = target.iter().any(|class| {
        class
            .refs
            .iter()
            .any(|r| fabric_client_holders.contains(&r.as_str()))
    });
    // Fabric's client model-loading API is a library slice as well: the
    // registry, its provider interfaces and the context a provider is handed.
    // A target that only registers a provider selects it, so a jar that never
    // touches models pays nothing.
    let fabric_model_holders = [
        FABRIC_MODEL_LOADING_REGISTRY,
        FABRIC_MODEL_RESOURCE_PROVIDER,
        FABRIC_MODEL_VARIANT_PROVIDER,
        FABRIC_MODEL_APPENDER,
        FABRIC_MODEL_PROVIDER_CONTEXT,
    ];
    let uses_fabric_models = target.iter().any(|class| {
        class
            .refs
            .iter()
            .any(|r| fabric_model_holders.contains(&r.as_str()))
    });
    // NeoForge's model events are nested classes of one library class, so the
    // outer name selects the pair.
    let uses_neoforge_models = uses_neoforge
        && target.iter().any(|class| {
            class
                .refs
                .iter()
                .any(|r| r.split('$').next() == Some(NEOFORGE_MODEL_EVENT))
        });
    // NeoForge's client event classes are library classes too. The loader
    // probe already guards the event bus they extend, so this only selects the
    // client slice (one extra file) for a target that names one of them.
    let uses_neoforge_client = uses_neoforge
        && target.iter().any(|class| {
            class.refs.iter().any(|r| {
                matches!(
                    r.as_str(),
                    "net/neoforged/neoforge/client/event/RegisterKeyMappingsEvent"
                        | "net/neoforged/neoforge/client/event/ClientTickEvent"
                        | "net/neoforged/neoforge/client/event/RenderGuiEvent"
                )
            })
        });
    // Fabric's client screen API: the ordered screen hooks and the widget
    // lookup helper. Neither draws a screen, so a target that never touches one
    // pays nothing.
    let uses_fabric_screen_events = names_fabric(FABRIC_SCREEN_EVENTS);
    let uses_fabric_screens = names_fabric(FABRIC_SCREENS);
    let uses_fabric_screen_api = uses_fabric_screen_events || uses_fabric_screens;
    // NeoForge's screen events are nested classes of one library class, so the
    // outer name selects `Init`/`Render` and their `Pre`/`Post` phases.
    let uses_neoforge_screens = uses_neoforge
        && target.iter().any(|class| {
            class
                .refs
                .iter()
                .any(|r| r.split('$').next() == Some(NEOFORGE_SCREEN_EVENT))
        });
    // Fabric API's event holders, command callback and the loader's bootstrap
    // queries are library classes too. They are only filled in below when the
    // target's jars do not carry the real class, so a real API jar is never
    // shadowed. The loader slice is emitted as a unit (see `FABRIC_LOADER_SHIMS`).
    let fabric_holders = [
        "net/fabricmc/fabric/api/event/Event",
        "net/fabricmc/fabric/api/event/lifecycle/v1/ServerLifecycleEvents",
        "net/fabricmc/fabric/api/event/lifecycle/v1/ClientLifecycleEvents",
        "net/fabricmc/fabric/api/event/player/ServerPlayerEvents",
        "net/fabricmc/fabric/api/command/v2/CommandRegistrationCallback",
    ];
    let uses_fabric_events = target.iter().any(|class| {
        class
            .refs
            .iter()
            .any(|r| fabric_holders.contains(&r.as_str()))
    });
    // Fabric's data-generation API (`fabric-datagen`) is a library slice too. A
    // datagen entrypoint only implements `DataGeneratorEntrypoint` and hands the
    // generator a pack, so the classes the slice emits select it by name; nothing
    // here runs or writes a generated file.
    let uses_fabric_datagen = target.iter().any(|class| {
        class.refs.iter().any(|r| {
            FABRIC_DATAGEN_SHIMS
                .iter()
                .any(|(name, ..)| *name == r.split('$').next().unwrap_or(r))
        })
    });
    // The datagen output names the loader's `ModContainer`; a datagen target
    // therefore always pulls the loader slice in with it.
    let uses_fabric_loader = uses_fabric_datagen
        || target.iter().any(|class| {
            class
                .refs
                .iter()
                .any(|r| r.starts_with("net/fabricmc/loader/api/"))
        });
    // Fabric's item-group registration and its client renderer registries are
    // library classes as well. Each is emitted only when the real API class is
    // absent (see `ClientShims::emit`), and only when the game types its
    // signature names resolve on this game jar.
    let fabric_item_group_holders = [
        "net/fabricmc/fabric/api/itemgroup/v1/ItemGroupEvents",
        "net/fabricmc/fabric/api/itemgroup/v1/FabricItemGroupEntries",
    ];
    let uses_fabric_item_groups = target.iter().any(|class| {
        class
            .refs
            .iter()
            .any(|r| fabric_item_group_holders.contains(&r.as_str()))
    });
    // A mod names the nested provider interfaces (`ModelLayerRegistry$
    // TexturedModelDataProvider`), so match the outer name as well.
    let fabric_renderer_holders = [
        "net/fabricmc/fabric/api/client/rendering/v1/EntityRendererRegistry",
        "net/fabricmc/fabric/api/client/rendering/v1/BlockEntityRendererRegistry",
        "net/fabricmc/fabric/api/client/rendering/v1/ModelLayerRegistry",
        "net/fabricmc/fabric/api/client/rendering/v1/ColorProviderRegistry",
    ];
    let uses_fabric_renderers = target.iter().any(|class| {
        class
            .refs
            .iter()
            .any(|r| fabric_renderer_holders.contains(&r.split('$').next().unwrap_or(r)))
    });
    // Model-layer registration is its own holder: its geometry roles are seeded
    // only when a mod names it, not for every renderer registry.
    let uses_model_layers = target.iter().any(|class| {
        class.refs.iter().any(|r| {
            r.split('$').next()
                == Some("net/fabricmc/fabric/api/client/rendering/v1/ModelLayerRegistry")
        })
    });
    // NeoForge's registration events: the creative-tab contents event and the
    // renderer registration event (a nested class, hence the `$` split).
    let neoforge_registration_holders = [
        "net/neoforged/neoforge/event/BuildCreativeModeTabContentsEvent",
        "net/neoforged/neoforge/client/event/EntityRenderersEvent",
    ];
    let uses_neoforge_registration = uses_neoforge
        && target.iter().any(|class| {
            class
                .refs
                .iter()
                .any(|r| neoforge_registration_holders.contains(&r.split('$').next().unwrap_or(r)))
        });
    // NeoForge entity capability and biome hooks are library classes. Emit
    // bounded markers only for jars that name one of the event surfaces.
    let uses_neoforge_entity_events = uses_neoforge
        && names_any(&[
            NEOFORGE_ENTITY_JOIN_LEVEL_EVENT,
            NEOFORGE_REGISTER_CAPABILITIES_EVENT,
            NEOFORGE_BIOME_MODIFIER,
            NEOFORGE_ISTORAGE,
        ]);
    let own: BTreeSet<&str> = target.iter().map(|c| c.name.as_str()).collect();
    // Shapes from dependency jars are real classes on javac/the probe
    // classpaths. Keep their names available when deciding whether a game
    // stub may extend or implement one (for example Fabric's entity builder
    // contract); they are still excluded from the generated façade by `own`.
    let mut available = lib_names.clone();
    available.extend(own.iter().map(|name| (*name).to_owned()));
    // Fabric's loader bootstrap values are one unit (`FABRIC_LOADER_SHIMS`): a
    // shim that names `ModContainer` may count on them when this compilation
    // writes all of them, and must report the type otherwise.
    let loader_slice_absent = FABRIC_LOADER_SHIMS
        .iter()
        .all(|(class, _)| !available.contains(*class));
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    for class in &target {
        for r in &class.refs {
            if (r.starts_with("net/minecraft/") || r.starts_with("com/mojang/"))
                && !own.contains(r.as_str())
                && !lib_names.contains(r)
            {
                wanted.insert(r.clone());
            }
        }
    }
    // A JVM descriptor can name a class through its UTF-8 descriptor alone;
    // javac does not have to emit a CONSTANT_Class entry for every parameter
    // type. Close the initial set over all member descriptors as well, or an
    // API callback such as `MobConversion(..., ConversionParams)` can compile
    // but fail with NoClassDefFoundError at runtime.
    for class in &target {
        for member in &class.members {
            for ty in descriptor_types(&member.descriptor) {
                if (ty.starts_with("net/minecraft/") || ty.starts_with("com/mojang/"))
                    && !own.contains(ty.as_str())
                    && !lib_names.contains(&ty)
                {
                    wanted.insert(ty);
                }
            }
        }
    }
    // The capability-provider and item-handler shims name the side a capability
    // is queried for and the stacks a handler moves, and a mod that only
    // registers a token or listens on the bus never mentions them.
    if uses_neoforge_entity_events || uses_legacy_forge {
        for name in [
            "net/minecraft/core/Direction",
            "net/minecraft/core/BlockPos",
            "net/minecraft/world/level/block/Block",
            "net/minecraft/world/level/block/entity/BlockEntity",
            "net/minecraft/world/level/block/entity/BlockEntityType",
            "net/minecraft/world/level/block/state/BlockState",
            "net/minecraft/world/level/Level",
            "net/minecraft/world/entity/Entity",
            "net/minecraft/world/entity/EntityType",
            "net/minecraft/world/item/ItemStack",
            "net/minecraft/world/item/Item",
        ] {
            if game.contains_key(name) && !lib_names.contains(name) && !own.contains(name) {
                wanted.insert(name.to_owned());
            }
        }
    }
    if uses_neoforge || uses_legacy_forge {
        for name in [
            "net/minecraft/network/RegistryFriendlyByteBuf",
            "net/minecraft/network/FriendlyByteBuf",
            "net/minecraft/network/codec/StreamCodec",
            "net/minecraft/network/protocol/common/custom/CustomPacketPayload",
            "net/minecraft/network/protocol/common/custom/CustomPacketPayload$Type",
            "net/minecraft/resources/Identifier",
        ] {
            if game.contains_key(name) && !lib_names.contains(name) && !own.contains(name) {
                wanted.insert(name.to_owned());
            }
        }
    }
    // The entity-event slice carries the joined entity and its level: the whole
    // unit is written whenever one of its surfaces is named, so both shapes seed
    // with it.
    if uses_neoforge_entity_events {
        for name in [
            "net/minecraft/world/entity/Entity",
            "net/minecraft/world/level/Level",
            "net/minecraft/core/RegistryAccess",
            "net/minecraft/core/Holder",
            "net/minecraft/world/level/biome/Biome",
            "net/minecraft/world/level/biome/BiomeGenerationSettings",
            "net/minecraft/world/level/biome/BiomeGenerationSettings$Builder",
        ] {
            if game.contains_key(name) && !lib_names.contains(name) && !own.contains(name) {
                wanted.insert(name.to_owned());
            }
        }
    }
    // The Fabric event shims name game types in their callback signatures, and
    // a mod that only writes `register(server -> {})` carries no reference to
    // them. Seed the façade with the types the emitted holders need; a
    // client-only type stays absent on a server-shaped jar and the holder that
    // names it is skipped below.
    if uses_fabric_events {
        for name in [
            "net/minecraft/server/MinecraftServer",
            "net/minecraft/client/Minecraft",
            "net/minecraft/server/level/ServerPlayer",
            "net/minecraft/commands/Commands",
            "net/minecraft/commands/Commands$CommandSelection",
            "net/minecraft/commands/CommandSourceStack",
            "net/minecraft/commands/CommandBuildContext",
        ] {
            if game.contains_key(name) && !lib_names.contains(name) && !own.contains(name) {
                wanted.insert(name.to_owned());
            }
        }
    }
    // The networking shims name the payload type and the connection's player.
    // A mod that only registers a handler never carries a reference to `Type`
    // itself, so seed the classes the emitted signatures name; `Type` pulls its
    // outer class in through the nested-class pass below.
    if uses_fabric_networking {
        for name in [
            "net/minecraft/network/protocol/common/custom/CustomPacketPayload",
            "net/minecraft/network/protocol/common/custom/CustomPacketPayload$Type",
            "net/minecraft/resources/Identifier",
            "net/minecraft/server/level/ServerPlayer",
            "net/minecraft/client/multiplayer/ClientPacketListener",
        ] {
            if game.contains_key(name) && !lib_names.contains(name) && !own.contains(name) {
                wanted.insert(name.to_owned());
            }
        }
        if let Some(name) = fabric_client_player {
            wanted.insert(name.to_owned());
        }
    }
    // The client and registration shims name game types in their signatures, and
    // a mod that writes `EVENT.register((context, ticks) -> {})` carries no
    // reference to them. Resolve each role against the game jar (26.3 renamed
    // `GuiGraphics` to `GuiGraphicsExtractor`, so the first candidate present
    // wins) and seed the real class so the shim compiles against it. A
    // server-shaped jar resolves nothing and the shims that need it are skipped
    // below, rather than fabricating client types a mod would then call into.
    let client = if uses_fabric_client
        || uses_neoforge_client
        || uses_fabric_screen_api
        || uses_neoforge_screens
        || uses_fabric_item_groups
        || uses_fabric_renderers
        || uses_neoforge_registration
        || uses_fabric_models
        || uses_neoforge_models
    {
        resolve_client_types(&game, &lib_names, &wanted)
    } else {
        BTreeMap::new()
    };
    // Only the roles an emitted shim actually names are seeded into the façade:
    // an item-group-only target must not drag every client GUI type in.
    let mut needed_roles: BTreeSet<&str> = BTreeSet::new();
    if uses_fabric_client || uses_neoforge_client {
        needed_roles.extend([
            "gui",
            "ticks",
            "minecraft",
            "key_mapping",
            "item_stack",
            "tooltip_context",
            "tooltip_type",
            "component",
        ]);
    }
    if uses_fabric_item_groups || uses_neoforge_registration {
        needed_roles.extend([
            "creative_mode_tab",
            "feature_flag_set",
            "item_stack",
            "resource_key",
        ]);
    }
    if uses_fabric_renderers || uses_neoforge_registration {
        needed_roles.extend([
            "block_entity_renderer_provider",
            "block_entity_type",
            "entity_renderer_provider",
            "entity_type",
        ]);
    }
    if uses_fabric_renderers {
        needed_roles.extend([
            "item",
            "item_stack",
            "block",
            "block_state",
            "block_pos",
            "block_and_tint_getter",
        ]);
    }
    if uses_model_layers {
        needed_roles.extend(["model_layer_location", "textured_model_data"]);
    }
    // The screen hooks name the client, the screen and the GUI the render
    // phases draw into; `Screens` also binds to the widget base, and the
    // NeoForge markers carry only the screen.
    if uses_fabric_screen_events {
        needed_roles.extend(["minecraft", "screen", "gui"]);
    }
    if uses_fabric_screens {
        needed_roles.extend(["screen", "clickable_widget"]);
    }
    if uses_neoforge_screens {
        needed_roles.insert("screen");
    }
    if uses_fabric_models {
        needed_roles.extend(["identifier", "unbaked_model", "baked_model", "model_loader"]);
    }
    if uses_neoforge_models {
        needed_roles.extend(["model_key", "baked_model"]);
    }
    for (role, name) in &client {
        if needed_roles.contains(role) {
            wanted.insert(name.clone());
        }
    }
    // The attachment shims name the id and the codec a registration carries.
    // Resolve them from the game jar and the dependency jars (26.3 calls the id
    // `Identifier`; a jar built against `ResourceLocation` still names it) and
    // seed the game class, so the shim compiles against the real type. A role
    // that resolves nowhere stays unresolved and the shim is reported below,
    // never emitted with an invented type.
    let attachment_types = if uses_fabric_attachments || uses_neoforge_attachments {
        resolve_attachment_types(&game, &lib_names, &own, &mut wanted)
    } else {
        BTreeMap::new()
    };
    // `AttachmentTarget` names `AttachmentType`, whose id accessor is the
    // resolved game type: without that role neither shim can compile, so the
    // interface is also not added to a game stub.
    let fabric_attachment_target =
        uses_fabric_attachments && attachment_types.contains_key("identifier");
    // The menu shims name the menu, its type, the player inventory, the codec
    // and (for a NeoForge extension) the buffer and id. Resolve them from the
    // game jar and the dependency jars and seed the game classes, so the shims
    // compile against the real stubs; an unresolved role is reported below.
    let menu_types = if uses_fabric_menus || uses_neoforge_menus {
        resolve_menu_types(&game, &lib_names, &own, &mut wanted)
    } else {
        BTreeMap::new()
    };
    // Fabric's datagen shims name the loader's `ModContainer` and the game's id.
    // A role that resolves nowhere is reported instead of being invented.
    let datagen_types = if uses_fabric_datagen {
        resolve_datagen_types(&game, &lib_names, &own, loader_slice_absent, &mut wanted)
    } else {
        BTreeMap::new()
    };
    let mut used: BTreeMap<String, BTreeSet<(String, String)>> = BTreeMap::new();
    for class in &target {
        for m in &class.members {
            if wanted.contains(&m.owner) {
                used.entry(m.owner.clone())
                    .or_default()
                    .insert((m.name.clone(), m.descriptor.clone()));
            }
        }
    }
    // The networking shims key handlers by `CustomPacketPayload.Type#id()`. A
    // mod that only hands a `Type` to `registerGlobalReceiver` never names that
    // accessor itself, so add it to the closure explicitly (it also pulls the
    // `Identifier` it returns into the façade).
    if uses_fabric_networking {
        used.entry(
            "net/minecraft/network/protocol/common/custom/CustomPacketPayload$Type".to_owned(),
        )
        .or_default()
        .insert((
            "id".to_owned(),
            "()Lnet/minecraft/resources/Identifier;".to_owned(),
        ));
    }
    // Accessor and invoker mixins are Java interfaces whose methods are
    // normally materialized by Mixin on the target class. The façade has no
    // bytecode weaver, so make the target implement those interfaces itself.
    // This keeps the common `(TargetAccessor) target` ABI alive while the
    // generated method bodies remain deliberately conservative defaults.
    let mut mixin_impls: BTreeMap<String, Vec<MixinImpl>> = BTreeMap::new();
    for mixin in target.iter().filter(|class| class.mixin) {
        // A package-private accessor cannot appear in a public façade class's
        // implements list. Keep the first generic slice to primitive/void
        // methods; object accessors frequently carry a Signature attribute
        // that the lightweight class reader intentionally does not decode,
        // so emitting a raw return type would fail javac's override checks.
        if mixin.access & 0x0201 != 0x0201 {
            continue;
        }
        let methods = mixin
            .declared
            .iter()
            .filter(|member| {
                member.kind == MemberKind::Method
                    && member.name != "<init>"
                    && safe_mixin_method(member)
                    && mixin
                        .method_annotations
                        .get(&member.name)
                        .is_some_and(|annotations| {
                            annotations.iter().any(|annotation| {
                                matches!(
                                    annotation_simple_name(&annotation.ty),
                                    "Accessor" | "Invoker"
                                )
                            })
                        })
            })
            .cloned()
            .collect::<Vec<_>>();
        if methods.is_empty()
            || methods.len()
                != mixin
                    .declared
                    .iter()
                    .filter(|member| {
                        member.kind == MemberKind::Method
                            && member.name != "<init>"
                            && mixin.method_annotations.get(&member.name).is_some_and(
                                |annotations| {
                                    annotations.iter().any(|annotation| {
                                        matches!(
                                            annotation_simple_name(&annotation.ty),
                                            "Accessor" | "Invoker"
                                        )
                                    })
                                },
                            )
                    })
                    .count()
        {
            continue;
        }
        for target_name in &mixin.mixin_targets {
            mixin_impls
                .entry(target_name.clone())
                .or_default()
                .push(MixinImpl {
                    interface: mixin.name.clone(),
                    methods: methods.clone(),
                });
        }
    }
    // Fabric's game classes carry the attachment store through its Mixin. The
    // façade has no weaver, so the game types a mod names implement that
    // interface directly (the same path an accessor Mixin takes): the stub
    // inherits its default methods and `((AttachmentTarget) entity)` links.
    if fabric_attachment_target {
        for target_name in FABRIC_ATTACHMENT_TARGETS {
            if wanted.contains(*target_name) {
                mixin_impls
                    .entry((*target_name).to_owned())
                    .or_default()
                    .push(MixinImpl {
                        interface: FABRIC_ATTACHMENT_TARGET.to_owned(),
                        methods: Vec::new(),
                    });
            }
        }
    }
    // NeoForge's menu extension is a Mixin interface on `MenuType` too: the
    // generated stub implements it directly (the same path), so a mod's
    // `((IMenuTypeExtension) type).naturalId()` links. Only when that interface
    // is the façade's own, never over a real NeoForge jar.
    let menu_type_extension =
        uses_neoforge_menus && !available.contains(NEOFORGE_MENU_TYPE_EXTENSION);
    if menu_type_extension && let Some(menu_type) = menu_types.get("menu_type") {
        mixin_impls
            .entry(menu_type.clone())
            .or_default()
            .push(MixinImpl {
                interface: NEOFORGE_MENU_TYPE_EXTENSION.to_owned(),
                methods: Vec::new(),
            });
    }
    // Bounded entity values that the generated bodies construct have to exist
    // in the same façade, or javac rejects the compilation unit before a mod
    // can run: `EntityType.getDimensions()` returns an `EntityDimensions`,
    // whose box is an `AABB` at a `Vec3`, and `SpawnPlacements` keeps its
    // registrations keyed by the placement and heightmap types. The closure
    // below injects them for every class it reaches, so a class that only
    // enters the set through another member's descriptor is covered too.
    // A stub cannot compile without the types it extends and names: close the
    // set over superclasses, interfaces and the descriptors of the members
    // that will be emitted.
    // A nested class needs its outer emitted around it.
    for name in wanted.clone() {
        if let Some((root, _)) = name.split_once('$') {
            wanted.insert(root.to_owned());
        }
    }
    // `ByteBufCodecs` is a catalogue: a mod names the constants it writes a
    // field with, and its payload codecs reach the rest through the façade's
    // own factory bodies. Every static factory is kept when the mod's code
    // touches the class at all, so a codec built inside the façade links the
    // buffer calls it makes on the mod's behalf.
    if let Some(factories) = game.get("net/minecraft/network/codec/ByteBufCodecs")
        && target.iter().any(|class| {
            class
                .refs
                .contains("net/minecraft/network/codec/ByteBufCodecs")
                && class.name != "net/minecraft/network/codec/ByteBufCodecs"
                && !own.contains(class.name.as_str())
        })
    {
        for member in &factories.declared {
            if member.kind == MemberKind::Method && member.is_static() {
                wanted.insert("net/minecraft/network/codec/ByteBufCodecs".to_owned());
                used.entry("net/minecraft/network/codec/ByteBufCodecs".to_owned())
                    .or_default()
                    .insert((member.name.clone(), member.descriptor.clone()));
            }
        }
    }
    let mut queue: Vec<String> = wanted.iter().cloned().collect();
    while let Some(name) = queue.pop() {
        let Some(class) = game.get(&name) else {
            continue;
        };
        let mut add = |ty: &str| {
            // Anything but the JDK: a missing library becomes an empty type
            // so the stubs still compile; a real one is on the classpath.
            let jdk = ["java/", "javax/", "jdk/", "sun/", "com/sun/"]
                .iter()
                .any(|p| ty.starts_with(p));
            if !jdk && !lib_names.contains(ty) && !own.contains(ty) && wanted.insert(ty.to_owned())
            {
                queue.push(ty.to_owned());
                // The outer class has to be rendered to hold it.
                if let Some((root, _)) = ty.split_once('$')
                    && wanted.insert(root.to_owned())
                {
                    queue.push(root.to_owned());
                }
            }
        };
        add(&class.superclass);
        for interface in &class.interfaces {
            add(interface);
        }
        let is_interface = class.access & 0x0200 != 0;
        let is_enum = class.access & 0x4000 != 0;
        let public = |m: &Declared| m.is_public() || m.is_protected();
        for member in &class.declared {
            // Must match what `render` emits, or a stub names a type it does
            // not have.
            let emitted = match member.kind {
                MemberKind::Field => !is_enum && public(member) && !member.is_enum_constant(),
                MemberKind::Method => {
                    (member.name == "<init>" && public(member))
                        || (public(member)
                            && (is_interface
                                || (class.access & 0x4000 == 0
                                    && class.superclass == "java/lang/Record")
                                || used.get(&name).is_some_and(|u| {
                                    u.contains(&(member.name.clone(), member.descriptor.clone()))
                                })))
                }
                _ => false,
            };
            if !emitted {
                continue;
            }
            for ty in descriptor_types(&member.descriptor) {
                add(&ty);
            }
        }
        // Snapshot-drift methods may be present only in `used`, so their
        // descriptor types are not covered by the game class's declaration.
        // Close over those synthetic signatures as well; otherwise javac can
        // fail on a dependency type before the JVM probe reaches the method.
        if let Some(methods) = used.get(&name) {
            for (_, descriptor) in methods {
                for ty in descriptor_types(descriptor) {
                    add(&ty);
                }
            }
        }
        for support in support_types(&name) {
            if game.contains_key(*support) {
                add(support);
            }
        }
    }
    // One compilation unit per outer class; nested classes are emitted inside
    // it, recursively (`Holder$Reference$Type` is two levels).
    let mut top: BTreeSet<String> = BTreeSet::new();
    let mut nested: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for name in &wanted {
        match name.rsplit_once('$') {
            None => {
                top.insert(name.clone());
            }
            Some((parent, simple)) => {
                nested
                    .entry(parent.to_owned())
                    .or_default()
                    .insert(simple.to_owned());
            }
        }
    }
    let src = out.join("src");
    let classes_dir = out.join("classes");
    let _ = fs::remove_dir_all(&src);
    let _ = fs::remove_dir_all(&classes_dir);
    fs::create_dir_all(&src).map_err(|e| e.to_string())?;
    fs::create_dir_all(&classes_dir).map_err(|e| e.to_string())?;
    let mut report = Report {
        sources: src.clone(),
        jar: out.join("facade.jar"),
        ..Report::default()
    };
    let mut files = Vec::new();
    // `render` gates an implemented interface on `available`. The attachment
    // interface is written as a shim below, so it is on this compilation's
    // classpath even when the real Fabric API jar is absent; the emission
    // guards keep reading the real `available` set.
    let mut compiling = available.clone();
    if fabric_attachment_target {
        compiling.insert(FABRIC_ATTACHMENT_TARGET.to_owned());
    }
    if menu_type_extension {
        compiling.insert(NEOFORGE_MENU_TYPE_EXTENSION.to_owned());
    }
    for root in &top {
        let source = tree(
            root,
            true,
            &game,
            &compiling,
            &used,
            &nested,
            &mixin_impls,
            &mut report,
        );
        let (dir, simple) = root.rsplit_once('/').unwrap_or(("", root.as_str()));
        let file = src.join(dir).join(format!("{simple}.java"));
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(&file, source).map_err(|e| format!("{}: {e}", file.display()))?;
        files.push(file);
    }
    // A few Fabric API accessor interfaces are deliberately only implemented
    // by Mixin at runtime. Keep the first bootstrap slice usable by emitting
    // the equivalent constant accessor into the façade jar, which is first on
    // the JVM classpath.
    let shim = src
        .join("net/fabricmc/fabric/mixin/attachment/ClientboundCustomPayloadPacketAccessor.java");
    if let Some(parent) = shim.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(
        &shim,
        "package net.fabricmc.fabric.mixin.attachment;\npublic interface ClientboundCustomPayloadPacketAccessor {\n    static int getMaxPayloadSize() { return 1048576; }\n}\n",
    )
    .map_err(|e| format!("{}: {e}", shim.display()))?;
    files.push(shim);
    // Fabric's VarInt accessor is another static constant used by data
    // attachment bootstrap.  It has no transformed instance target, so the
    // vanilla bound is safe to expose directly from the façade.
    let varint_shim = src.join("net/fabricmc/fabric/mixin/attachment/VarIntAccessor.java");
    if let Some(parent) = varint_shim.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(
        &varint_shim,
        "package net.fabricmc.fabric.mixin.attachment;\npublic interface VarIntAccessor {\n    static int getMaxByteSize() { return 5; }\n}\n",
    )
    .map_err(|e| format!("{}: {e}", varint_shim.display()))?;
    files.push(varint_shim);
    // This accessor is client-only. A server-shaped game jar does not carry
    // ModelLayerLocation, so emitting it there would make an otherwise valid
    // façade fail javac before any mod code is loaded.
    let model_layers_accessor = "net/fabricmc/fabric/mixin/client/rendering/ModelLayersAccessor";
    if available.contains(model_layers_accessor)
        && game.contains_key("net/minecraft/client/model/geom/ModelLayerLocation")
    {
        let model_layers_shim =
            src.join("net/fabricmc/fabric/mixin/client/rendering/ModelLayersAccessor.java");
        if let Some(parent) = model_layers_shim.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(
            &model_layers_shim,
            "package net.fabricmc.fabric.mixin.client.rendering;\npublic interface ModelLayersAccessor {\n    static java.util.Set<net.minecraft.client.model.geom.ModelLayerLocation> getLayers() { return new java.util.HashSet<>(); }\n}\n",
        )
        .map_err(|e| format!("{}: {e}", model_layers_shim.display()))?;
        files.push(model_layers_shim);
    }
    // The callback interfaces `StreamCodec.of` takes, and the operation
    // `apply` takes. They are written whenever this classpath does not carry
    // them: the façade's own codec names them in its signatures, and a real
    // game classpath's copies come first at runtime.
    if let Some(stream_codec) = game.get("net/minecraft/network/codec/StreamCodec") {
        for (class, source) in [
            ("net/minecraft/network/codec/StreamEncoder", STREAM_ENCODER),
            ("net/minecraft/network/codec/StreamDecoder", STREAM_DECODER),
            (
                "net/minecraft/network/codec/StreamMemberEncoder",
                STREAM_MEMBER_ENCODER,
            ),
        ] {
            if !available.contains(class) {
                write_shim(&mut files, &src, class, source)?;
            }
        }
        // `StreamCodec.CodecOperation` is nested in the game's interface, which
        // the façade writes with `default` bodies; a nested abstract member
        // would shadow the name a mod writes (`codec -> ...` needs a target),
        // so the bounded slice carries it as a top-level file.
        if !available.contains(STREAM_CODEC_OPERATION_TYPE)
            && !game.contains_key(STREAM_CODEC_OPERATION_TYPE)
            && stream_codec
                .declared
                .iter()
                .any(|member| member.kind == MemberKind::Method && member.name == "apply")
        {
            // The file is named after the type `javac` finds in it: a public
            // top-level type cannot carry the `$` of the game's nested shape.
            write_shim(
                &mut files,
                &src,
                STREAM_CODEC_OPERATION_TYPE_FILE,
                STREAM_CODEC_OPERATION,
            )?;
        }
    }
    // Fabric API's `PayloadTypeRegistry` is a library class: when the API jar
    // is part of the target's classpath it stays real, and this bounded marker
    // only fills the jar-only slice where `playS2C()/playC2S()` would
    // otherwise fail to link.
    let payload_registry = "net/fabricmc/fabric/api/networking/v1/PayloadTypeRegistry";
    if uses_fabric_payloads && !available.contains(payload_registry) {
        let payload_registry_file = src.join(format!("{payload_registry}.java"));
        if let Some(parent) = payload_registry_file.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        fs::write(&payload_registry_file, FABRIC_PAYLOAD_TYPE_REGISTRY)
            .map_err(|e| format!("{}: {e}", payload_registry_file.display()))?;
        files.push(payload_registry_file);
        report.bound_legacy(
            payload_registry,
            "PayloadTypeRegistrar.register",
            "the jar-only slice must not name CustomPacketPayload.Type/StreamCodec, so the payload and codec stay Object",
        );
    }
    // Fabric API's play-stage networking: the receiver maps are real (they key
    // handlers by payload type id and `canSend` reads them), while `send` and
    // `sendToServer` are bounded markers that record the type id and direction.
    // Each class is emitted only when the real API class is absent and every
    // game type its signatures name resolves on this jar.
    if uses_fabric_networking {
        let payload = "net/minecraft/network/protocol/common/custom/CustomPacketPayload";
        let payload_type = "net/minecraft/network/protocol/common/custom/CustomPacketPayload$Type";
        emit_networking_shim(
            &mut files,
            &mut report,
            &src,
            &available,
            &game,
            "net/fabricmc/fabric/api/networking/v1/PacketSender",
            &[payload],
            || FABRIC_PACKET_SENDER.to_owned(),
        )?;
        if uses_server_networking {
            emit_networking_shim(
                &mut files,
                &mut report,
                &src,
                &available,
                &game,
                "net/fabricmc/fabric/api/networking/v1/ServerPlayNetworking",
                &[
                    payload,
                    payload_type,
                    "net/minecraft/server/level/ServerPlayer",
                    "net/minecraft/resources/Identifier",
                ],
                || FABRIC_SERVER_PLAY_NETWORKING.to_owned(),
            )?;
        }
        let client = "net/fabricmc/fabric/api/client/networking/v1/ClientPlayNetworking";
        if uses_client_networking && !available.contains(client) {
            match fabric_client_player {
                Some(player) => emit_networking_shim(
                    &mut files,
                    &mut report,
                    &src,
                    &available,
                    &game,
                    client,
                    &[
                        payload,
                        payload_type,
                        "net/minecraft/client/multiplayer/ClientPacketListener",
                        player,
                        "net/minecraft/resources/Identifier",
                    ],
                    || FABRIC_CLIENT_PLAY_NETWORKING.replace("{player}", &java_name(player)),
                )?,
                None => report.missing.push(format!(
                    "{client} (needs client type net/minecraft/client/player/LocalPlayer)"
                )),
            }
        }
    }
    // Fabric API's lifecycle/player event holders and the command registration
    // callback: registration order is preserved, `invoker()` returns the
    // combined handler, and no gameplay runs. A holder is only emitted when its
    // callback signatures can actually resolve against this façade.
    if uses_fabric_events {
        let event_base = "net/fabricmc/fabric/api/event/Event";
        // `Event.horizon` is this slice's factory: when a real `Event` interface
        // is on the classpath it has no such member, so the holders that build
        // on it are skipped rather than shadowing half of the API.
        let base_absent = !available.contains(event_base);
        if base_absent {
            write_shim(&mut files, &src, event_base, FABRIC_EVENT)?;
        }
        let compilable =
            |ty: &str| wanted.contains(ty) || lib_names.contains(ty) || own.contains(ty);
        for (class, doc, events, requires) in [
            (
                "net/fabricmc/fabric/api/event/lifecycle/v1/ServerLifecycleEvents",
                "Bounded server lifecycle events: ordered listeners, invoked by the host.",
                FABRIC_SERVER_LIFECYCLE,
                &["net/minecraft/server/MinecraftServer"][..],
            ),
            (
                "net/fabricmc/fabric/api/event/lifecycle/v1/ClientLifecycleEvents",
                "Bounded client lifecycle events: ordered listeners, invoked by the host.",
                FABRIC_CLIENT_LIFECYCLE,
                &["net/minecraft/client/Minecraft"][..],
            ),
            (
                "net/fabricmc/fabric/api/event/player/ServerPlayerEvents",
                "Bounded server player events: ordered listeners, invoked by the host.",
                FABRIC_SERVER_PLAYER_EVENTS,
                &["net/minecraft/server/level/ServerPlayer"][..],
            ),
        ] {
            if available.contains(class) {
                continue;
            }
            if !base_absent {
                report
                    .missing
                    .push(format!("{class} (needs {event_base} absent)"));
                continue;
            }
            let missing: Vec<&str> = requires
                .iter()
                .copied()
                .filter(|ty| !compilable(ty))
                .collect();
            if !missing.is_empty() {
                report
                    .missing
                    .push(format!("{class} (needs {})", missing.join(", ")));
                continue;
            }
            let (package, simple) = class.rsplit_once('/').unwrap_or(("", class));
            let source = fabric_event_source(package, simple, doc, events);
            write_shim(&mut files, &src, class, &source)?;
        }
        // Brigadier is a library class: a generated empty stub would lose its
        // type parameters, so the command callback only lands when the real
        // dispatcher is on the classpath.
        let command = "net/fabricmc/fabric/api/command/v2/CommandRegistrationCallback";
        if base_absent
            && !available.contains(command)
            && lib_names.contains("com/mojang/brigadier/CommandDispatcher")
            && compilable("net/minecraft/commands/CommandSourceStack")
            && compilable("net/minecraft/commands/CommandBuildContext")
            && wanted.contains("net/minecraft/commands/Commands$CommandSelection")
        {
            write_shim(
                &mut files,
                &src,
                command,
                FABRIC_COMMAND_REGISTRATION_CALLBACK,
            )?;
        }
    }
    // Fabric API's client bootstrap surface: the same bounded `Event` dispatch
    // as the server holders (registration order, `invoker()` returns the
    // combined handler), plus the key-binding registry. Each class is emitted
    // only when the real API class is absent, and only when the client types
    // its signature names resolved against the game jar.
    let client_shims = ClientShims {
        src: &src,
        available: &available,
        types: &client,
    };
    let fill_client = |template: &str| fill_types(template, &client);
    if uses_fabric_client {
        let key_binding = "net/fabricmc/fabric/api/client/keybinding/v1/KeyBindingHelper";
        client_shims.emit(
            &mut files,
            &mut report,
            key_binding,
            &["key_mapping"],
            || fill_client(FABRIC_KEY_BINDING_HELPER),
        )?;
        let event_base = "net/fabricmc/fabric/api/event/Event";
        // `Event.horizon` is this slice's factory, so a real `Event` interface
        // (no such member) leaves the three event holders out instead of
        // shadowing half of the API.
        if available.contains(event_base) {
            for class in [
                "net/fabricmc/fabric/api/client/rendering/v1/HudRenderCallback",
                "net/fabricmc/fabric/api/client/event/lifecycle/v1/ClientTickEvents",
                "net/fabricmc/fabric/api/client/item/tooltip/v1/ItemTooltipCallback",
            ] {
                if !available.contains(class) {
                    report
                        .missing
                        .push(format!("{class} (needs {event_base} absent)"));
                }
            }
        } else {
            if !uses_fabric_events {
                write_shim(&mut files, &src, event_base, FABRIC_EVENT)?;
            }
            let hud = "net/fabricmc/fabric/api/client/rendering/v1/HudRenderCallback";
            client_shims.emit(&mut files, &mut report, hud, &["gui", "ticks"], || {
                fill_client(FABRIC_HUD_RENDER_CALLBACK)
            })?;
            let ticks = "net/fabricmc/fabric/api/client/event/lifecycle/v1/ClientTickEvents";
            client_shims.emit(&mut files, &mut report, ticks, &["minecraft"], || {
                // The generator names the client class fully qualified in
                // the signature and the invoker lambda; take the resolved
                // 26.3 name rather than assuming `Minecraft`.
                fabric_event_source(
                    "net.fabricmc.fabric.api.client.event.lifecycle.v1",
                    "ClientTickEvents",
                    "Bounded client tick events: ordered listeners, invoked by the host.",
                    FABRIC_CLIENT_TICK_EVENTS,
                )
                .replace(
                    "net.minecraft.client.Minecraft",
                    &java_name(&client["minecraft"]),
                )
            })?;
            let tooltip = "net/fabricmc/fabric/api/client/item/tooltip/v1/ItemTooltipCallback";
            client_shims.emit(
                &mut files,
                &mut report,
                tooltip,
                &["item_stack", "tooltip_context", "tooltip_type", "component"],
                || fill_client(FABRIC_ITEM_TOOLTIP_CALLBACK),
            )?;
        }
    }
    // Fabric's client screen API: the ordered hooks a mod attaches to a
    // screen's lifecycle (`AFTER_INIT`'s four-argument callback is the one a
    // ported screen registers) and the widget lookups. The host posts the hooks
    // and owns the widget tree.
    if uses_fabric_screen_events {
        let event_base = "net/fabricmc/fabric/api/event/Event";
        if available.contains(event_base) {
            if !available.contains(FABRIC_SCREEN_EVENTS) {
                report.missing.push(format!(
                    "{FABRIC_SCREEN_EVENTS} (needs {event_base} absent)"
                ));
            }
        } else {
            // `Event.horizon` is this slice's factory; the event base may have
            // been written by the server, client or item-group slice already.
            let event_file = format!("{event_base}.java");
            if !files.iter().any(|file| file.ends_with(&event_file)) {
                write_shim(&mut files, &src, event_base, FABRIC_EVENT)?;
            }
            client_shims.emit(
                &mut files,
                &mut report,
                FABRIC_SCREEN_EVENTS,
                &["minecraft", "screen", "gui"],
                || fill_client(FABRIC_SCREEN_EVENTS_SOURCE),
            )?;
        }
    }
    if uses_fabric_screens {
        client_shims.emit(
            &mut files,
            &mut report,
            FABRIC_SCREENS,
            &["screen", "clickable_widget"],
            || fill_client(FABRIC_SCREENS_SOURCE),
        )?;
    }
    // Fabric API's item-group registration: the same bounded `Event` shape as
    // the client holders (registration order, `invoker()` walks them), one per
    // tab, plus the ordered entry list a listener edits. Emitted only for an
    // absent API class whose game types resolved on this jar.
    if uses_fabric_item_groups {
        let event_base = "net/fabricmc/fabric/api/event/Event";
        if available.contains(event_base) {
            report.missing.push(format!(
                "net/fabricmc/fabric/api/itemgroup/v1/ItemGroupEvents (needs {event_base} absent)"
            ));
        } else {
            // `Event.horizon` is this slice's factory, and the client slice
            // above writes it only for its own flags; write it here unless it
            // is already in the source list.
            let event_file = format!("{event_base}.java");
            if !files.iter().any(|file| file.ends_with(&event_file)) {
                write_shim(&mut files, &src, event_base, FABRIC_EVENT)?;
            }
            client_shims.emit(
                &mut files,
                &mut report,
                "net/fabricmc/fabric/api/itemgroup/v1/ItemGroupEvents",
                &["creative_mode_tab", "feature_flag_set", "resource_key"],
                || fill_client(FABRIC_ITEM_GROUP_EVENTS),
            )?;
            client_shims.emit(
                &mut files,
                &mut report,
                "net/fabricmc/fabric/api/itemgroup/v1/FabricItemGroupEntries",
                &["item_stack"],
                || fill_client(FABRIC_ITEM_GROUP_ENTRIES),
            )?;
        }
    }
    // Fabric's client renderer registries retain what a mod registers; the host
    // owns rendering, so these are lists, not renderers.
    if uses_fabric_renderers {
        for (class, roles, template) in [
            (
                "net/fabricmc/fabric/api/client/rendering/v1/EntityRendererRegistry",
                &["entity_type", "entity_renderer_provider"][..],
                FABRIC_ENTITY_RENDERER_REGISTRY,
            ),
            (
                "net/fabricmc/fabric/api/client/rendering/v1/BlockEntityRendererRegistry",
                &["block_entity_type", "block_entity_renderer_provider"][..],
                FABRIC_BLOCK_ENTITY_RENDERER_REGISTRY,
            ),
            (
                "net/fabricmc/fabric/api/client/rendering/v1/ModelLayerRegistry",
                &["model_layer_location", "textured_model_data"][..],
                FABRIC_MODEL_LAYER_REGISTRY,
            ),
            (
                "net/fabricmc/fabric/api/client/rendering/v1/ColorProviderRegistry",
                &[
                    "item",
                    "item_stack",
                    "block",
                    "block_state",
                    "block_pos",
                    "block_and_tint_getter",
                ][..],
                FABRIC_COLOR_PROVIDER_REGISTRY,
            ),
        ] {
            // The model-layer registry names model roles a target that only
            // touches another renderer registry never seeds; emit it only for
            // a target that names it, or javac sees classes this source set
            // does not carry.
            if class.ends_with("ModelLayerRegistry") && !uses_model_layers {
                continue;
            }
            client_shims.emit(&mut files, &mut report, class, roles, || {
                fill_client(template)
            })?;
        }
    }
    // Fabric's client model-loading API is one unit: the registry names the
    // provider interfaces, and a provider names the context. The host owns
    // baking, so every class either lands whole or reports the roles this jar
    // lacks, and a real API class is never shadowed.
    if uses_fabric_models {
        let mut model_types = client.clone();
        // The context is the façade's own class, not a game role: a provider's
        // signature names it, so it joins the fill-in map.
        model_types.insert(
            "model_provider_context",
            FABRIC_MODEL_PROVIDER_CONTEXT.to_owned(),
        );
        let missing: Vec<&str> = ["identifier", "baked_model", "model_loader", "unbaked_model"]
            .into_iter()
            .filter(|role| !model_types.contains_key(role))
            .collect();
        if missing.is_empty() {
            for (class, template) in [
                (
                    FABRIC_MODEL_PROVIDER_CONTEXT,
                    FABRIC_MODEL_PROVIDER_CONTEXT_SOURCE,
                ),
                (
                    FABRIC_MODEL_RESOURCE_PROVIDER,
                    FABRIC_MODEL_RESOURCE_PROVIDER_SOURCE,
                ),
                (
                    FABRIC_MODEL_VARIANT_PROVIDER,
                    FABRIC_MODEL_VARIANT_PROVIDER_SOURCE,
                ),
                (FABRIC_MODEL_APPENDER, FABRIC_MODEL_APPENDER_SOURCE),
                (
                    FABRIC_MODEL_LOADING_REGISTRY,
                    FABRIC_MODEL_LOADING_REGISTRY_SOURCE,
                ),
            ] {
                if !available.contains(class) {
                    write_shim(&mut files, &src, class, &fill_types(template, &model_types))?;
                }
            }
        } else {
            report.missing.push(format!(
                "net/fabricmc/fabric/api/client/model/loading/v1 (needs {})",
                missing.join(", ")
            ));
        }
    }
    // Fabric's data-attachment API: one registration per attachment id, kept in
    // a deterministic map with its codec and initializer markers plus the
    // persistent and copy-on-death flags. `AttachmentType#id()` answers the
    // identifier it was registered under, and `AttachmentTarget` keeps attached
    // values per target in the interface's bounded store. Nothing is serialized
    // or moved to a live target here.
    if uses_fabric_attachments {
        // Every row names the id role: `AttachmentTarget` inherits the
        // `AttachmentType` interface, so it needs the same resolved id type
        // even though its own methods only name that interface.
        let attachment_type = "net/fabricmc/fabric/api/attachment/v1/AttachmentType";
        let registry = "net/fabricmc/fabric/api/attachment/v1/AttachmentRegistry";
        for (class, requires, template) in [
            (attachment_type, &["identifier"][..], FABRIC_ATTACHMENT_TYPE),
            (
                FABRIC_ATTACHMENT_TARGET,
                &["identifier"][..],
                FABRIC_ATTACHMENT_TARGET_SOURCE,
            ),
        ] {
            emit_role_shim(
                &mut files,
                &mut report,
                &src,
                &available,
                &attachment_types,
                class,
                requires,
                || fill_types(template, &attachment_types),
            )?;
        }
        // `buildAndRegister` answers the `AttachmentType` this slice emits: a
        // real type with members the shim does not model would mix two shapes,
        // so the registry lands only when the type is the façade's own.
        if available.contains(attachment_type) {
            report
                .missing
                .push(format!("{registry} (needs {attachment_type} absent)"));
        } else {
            emit_role_shim(
                &mut files,
                &mut report,
                &src,
                &available,
                &attachment_types,
                registry,
                &["identifier", "codec"],
                || fill_types(FABRIC_ATTACHMENT_REGISTRY, &attachment_types),
            )?;
        }
    }
    // The loader slice is one unit: the bounded containers name each other, so
    // a partial real jar would mix two shapes. Emit nothing when any of it is
    // already on the classpath.
    if uses_fabric_loader
        && FABRIC_LOADER_SHIMS
            .iter()
            .all(|(class, _)| !available.contains(*class))
    {
        for (class, source) in FABRIC_LOADER_SHIMS {
            write_shim(&mut files, &src, class, source)?;
        }
    }
    // Fabric's data-generation API: the `fabric-datagen` entrypoint, the
    // generator it is handed, the output a provider receives and the pack that
    // retains providers. Datagen is a build-time step, so `addProvider` only
    // records one `{pack, id, kind, provider}` row in call order; nothing is
    // generated and no real API class is shadowed.
    if uses_fabric_datagen {
        let missing: Vec<&str> = ["mod_container", "identifier"]
            .into_iter()
            .filter(|role| !datagen_types.contains_key(role))
            .collect();
        if !missing.is_empty() {
            report.missing.push(format!(
                "net/fabricmc/fabric/api/datagen/v1 (needs {})",
                missing.join(", ")
            ));
        } else {
            for (class, source, requires) in FABRIC_DATAGEN_SHIMS {
                emit_role_shim(
                    &mut files,
                    &mut report,
                    &src,
                    &available,
                    &datagen_types,
                    class,
                    requires,
                    || fill_types(source, &datagen_types),
                )?;
            }
        }
    }
    if uses_neoforge || uses_legacy_forge {
        // 26.3's `Registry` is an interface (and its façade stub drops the type
        // parameter): a shim that builds one needs an anonymous implementation,
        // and one that extends it cannot name a class or a type argument.
        let game_registry_interface = game
            .get("net/minecraft/core/Registry")
            .is_some_and(|class| class.access & 0x0200 != 0);
        // Whether `Registry` in this compilation is one of these shims (a
        // generic class) or the game's own stub.
        let registry_is_shim = !(wanted.contains("net/minecraft/core/Registry")
            || lib_names.contains("net/minecraft/core/Registry"));
        let defaulted_registry = if registry_is_shim {
            concat!(
                "package net.minecraft.core;\n",
                "import net.minecraft.resources.Identifier;\n",
                "public class DefaultedRegistry<T> extends Registry<T> {\n",
                "    public Identifier getKey(Object value) { return super.getKey(value); }\n",
                "}\n",
            )
            .to_owned()
        } else if game_registry_interface {
            concat!(
                "package net.minecraft.core;\n",
                "import net.minecraft.resources.Identifier;\n",
                "public interface DefaultedRegistry<T> extends Registry {\n",
                "    Identifier getKey(Object value);\n",
                "}\n",
            )
            .to_owned()
        } else {
            // The game's `Registry` is a class stub: it already declares
            // `getKey`, and the stub drops type parameters.
            String::new()
        };
        let register_event = concat!(
            "package net.neoforged.neoforge.registries;\n",
            "import java.util.function.Consumer;\n",
            "import java.util.function.Supplier;\n",
            "import net.minecraft.core.Registry;\n",
            "import net.minecraft.resources.Identifier;\n",
            "import net.minecraft.resources.ResourceKey;\n",
            "import net.neoforged.bus.api.Event;\n",
            "public class RegisterEvent extends Event {\n",
            "    private final ResourceKey horizonRegistryKey = new ResourceKey();\n",
            "    private final Registry horizonRegistry = __HORIZON_REGISTRY_INIT__;\n",
            "    public void register(ResourceKey key, Identifier id, Supplier value) { if (value != null) horizonRegistry.register(key, id, value.get()); }\n",
            "    public void register(ResourceKey key, Consumer callback) { if (callback != null) callback.accept(new RegisterHelper() { public void register(Identifier id, Object value) { horizonRegistry.register(key, id, value); } }); }\n",
            "    public ResourceKey getRegistryKey() { return horizonRegistryKey; }\n",
            "    public Registry getRegistry() { return horizonRegistry; }\n",
            "    public Registry getRegistry(ResourceKey key) { return horizonRegistry; }\n",
            "    public interface RegisterHelper { default void register(ResourceKey key, Object value) {} void register(Identifier id, Object value); }\n",
            "}\n",
        )
        .replace(
            "__HORIZON_REGISTRY_INIT__",
            if game_registry_interface {
                "new Registry() {}"
            } else {
                "new Registry()"
            },
        );
        let shims = [
            (
                "net/neoforged/fml/common/Mod.java",
                concat!(
                    "package net.neoforged.fml.common;\n",
                    "import java.lang.annotation.ElementType;\n",
                    "import java.lang.annotation.Retention;\n",
                    "import java.lang.annotation.RetentionPolicy;\n",
                    "import java.lang.annotation.Target;\n",
                    "@Retention(RetentionPolicy.RUNTIME) @Target(ElementType.TYPE)\n",
                    "public @interface Mod { String value(); }\n",
                ),
            ),
            ("net/neoforged/bus/api/Event.java", NEOFORGE_EVENT),
            (
                "net/neoforged/bus/api/SubscribeEvent.java",
                NEOFORGE_SUBSCRIBE_EVENT,
            ),
            (
                "net/neoforged/bus/api/IEventBus.java",
                concat!(
                    "package net.neoforged.bus.api;\n",
                    "import java.util.function.Consumer;\n",
                    "public interface IEventBus {\n",
                    "    default IEventBus addListener(Consumer<?> listener) { return this; }\n",
                    "    default IEventBus addListener(EventPriority priority, Consumer<?> listener) { return this; }\n",
                    "    default IEventBus addListener(EventPriority priority, boolean receiveCancelled, Consumer<?> listener) { return this; }\n",
                    "    default <T extends Event> IEventBus addListener(Class<T> type, Consumer<T> listener) { return this; }\n",
                    "    default <T extends Event> IEventBus addListener(EventPriority priority, Class<T> type, Consumer<T> listener) { return this; }\n",
                    "    default <T extends Event> IEventBus addListener(EventPriority priority, boolean receiveCancelled, Class<T> type, Consumer<T> listener) { return this; }\n",
                    "    default IEventBus register(Object object) { return this; }\n",
                    "    default IEventBus unregister(Object object) { return this; }\n",
                    "    default boolean post(Event event) { return false; }\n",
                    "}\n",
                ),
            ),
            (
                "net/neoforged/bus/api/EventPriority.java",
                concat!(
                    "package net.neoforged.bus.api;\n",
                    "public enum EventPriority { HIGHEST, HIGH, NORMAL, LOW, LOWEST }\n",
                ),
            ),
            // Forge 1.20.x kept the event-bus ABI in its own package.  These
            // aliases intentionally mirror only the registration and posting
            // surface used during mod bootstrap; dispatch is recorded by the
            // native host and unsupported listener invocation remains a
            // diagnostic boundary.
            ("net/minecraftforge/eventbus/api/Event.java", FORGE_EVENT),
            (
                "net/minecraftforge/eventbus/api/EventPriority.java",
                "package net.minecraftforge.eventbus.api; public enum EventPriority { HIGHEST, HIGH, NORMAL, LOW, LOWEST }\n",
            ),
            (
                "net/minecraftforge/eventbus/api/SubscribeEvent.java",
                concat!(
                    "package net.minecraftforge.eventbus.api;\n",
                    "import java.lang.annotation.ElementType;\n",
                    "import java.lang.annotation.Retention;\n",
                    "import java.lang.annotation.RetentionPolicy;\n",
                    "import java.lang.annotation.Target;\n",
                    "@Retention(RetentionPolicy.RUNTIME) @Target(ElementType.METHOD)\n",
                    "public @interface SubscribeEvent { EventPriority priority() default EventPriority.NORMAL; boolean receiveCanceled() default false; }\n",
                ),
            ),
            (
                "net/minecraftforge/eventbus/api/IEventBus.java",
                concat!(
                    "package net.minecraftforge.eventbus.api;\n",
                    "import java.util.function.Consumer;\n",
                    "public interface IEventBus {\n",
                    "    default IEventBus addListener(Consumer<?> listener) { return this; }\n",
                    "    default IEventBus addListener(EventPriority priority, Consumer<?> listener) { return this; }\n",
                    "    default IEventBus addListener(EventPriority priority, boolean receiveCancelled, Consumer<?> listener) { return this; }\n",
                    "    default <T extends Event> IEventBus addListener(Class<T> type, Consumer<T> listener) { return this; }\n",
                    "    default <T extends Event> IEventBus addListener(EventPriority priority, Class<T> type, Consumer<T> listener) { return this; }\n",
                    "    default <T extends Event> IEventBus addListener(EventPriority priority, boolean receiveCancelled, Class<T> type, Consumer<T> listener) { return this; }\n",
                    "    default IEventBus register(Object object) { return this; }\n",
                    "    default IEventBus unregister(Object object) { return this; }\n",
                    "    default boolean post(Event event) { return false; }\n",
                    "}\n",
                ),
            ),
            (
                "net/minecraftforge/common/MinecraftForge.java",
                concat!(
                    "package net.minecraftforge.common;\n",
                    "import net.minecraftforge.eventbus.api.IEventBus;\n",
                    "public final class MinecraftForge {\n",
                    "    private MinecraftForge() {}\n",
                    "    public static final IEventBus EVENT_BUS = new IEventBus() {};\n",
                    "}\n",
                ),
            ),
            (
                "net/neoforged/neoforge/capabilities/CapabilityToken.java",
                concat!(
                    "package net.neoforged.neoforge.capabilities;\n",
                    "public class CapabilityToken<T> { public CapabilityToken() {} }\n",
                ),
            ),
            (
                "net/neoforged/neoforge/capabilities/Capability.java",
                concat!(
                    "package net.neoforged.neoforge.capabilities;\n",
                    "import net.neoforged.neoforge.common.util.LazyOptional;\n",
                    "public class Capability<T> {\n",
                    "    public boolean isRegistered() { return true; }\n",
                    "    public <R> LazyOptional<R> orEmpty(R value, Object requested) { return requested == this && value != null ? LazyOptional.of(() -> value) : LazyOptional.empty(); }\n",
                    "}\n",
                ),
            ),
            (
                "net/neoforged/neoforge/capabilities/CapabilityManager.java",
                concat!(
                    "package net.neoforged.neoforge.capabilities;\n",
                    "public final class CapabilityManager {\n",
                    "    private CapabilityManager() {}\n",
                    "    public static <T> Capability<T> get(CapabilityToken<T> token) { return new Capability<>(); }\n",
                    "}\n",
                ),
            ),
            (
                "net/neoforged/neoforge/common/util/LazyOptional.java",
                concat!(
                    "package net.neoforged.neoforge.common.util;\n",
                    "import java.util.Optional;\n",
                    "import java.util.function.Consumer;\n",
                    "import java.util.function.Function;\n",
                    "import java.util.function.Supplier;\n",
                    "public class LazyOptional<T> {\n",
                    "    private Supplier<? extends T> horizonSupplier;\n",
                    "    private T horizonValue;\n",
                    "    private boolean horizonValid = true;\n",
                    "    private LazyOptional(Supplier<? extends T> supplier) { this.horizonSupplier = supplier; }\n",
                    "    public static <T> LazyOptional<T> empty() { return new LazyOptional<>(() -> null); }\n",
                    "    public static <T> LazyOptional<T> of(Supplier<? extends T> supplier) { return new LazyOptional<>(supplier); }\n",
                    "    public static <T> LazyOptional<T> ofNullable(T value) { return new LazyOptional<>(() -> value); }\n",
                    "    private T horizonGet() { if (horizonValue == null && horizonSupplier != null) horizonValue = horizonSupplier.get(); return horizonValue; }\n",
                    "    public boolean isPresent() { return horizonValid && horizonGet() != null; }\n",
                    "    public T resolve() { return isPresent() ? horizonGet() : null; }\n",
                    "    public T orElse(T fallback) { T value = resolve(); return value == null ? fallback : value; }\n",
                    "    public T orElseGet(Supplier<? extends T> fallback) { T value = resolve(); return value == null ? fallback.get() : value; }\n",
                    "    public void ifPresent(Consumer<? super T> consumer) { T value = resolve(); if (value != null) consumer.accept(value); }\n",
                    "    public <R> LazyOptional<R> map(Function<? super T, ? extends R> mapper) { T value = resolve(); return value == null ? empty() : of(() -> mapper.apply(value)); }\n",
                    "    @SuppressWarnings(\"unchecked\") public <R> LazyOptional<R> cast() { return (LazyOptional<R>) this; }\n",
                    "    public void invalidate() { horizonValid = false; horizonValue = null; horizonSupplier = () -> null; }\n",
                    "    public LazyOptional<T> addListener(Consumer<LazyOptional<T>> listener) { if (!horizonValid) listener.accept(this); return this; }\n",
                    "    public Optional<T> resolveOptional() { return Optional.ofNullable(resolve()); }\n",
                    "}\n",
                ),
            ),
            // Legacy Forge capability package aliases. Keep these bounded to
            // the same marker and optional semantics as the NeoForge shims.
            (
                "net/minecraftforge/common/capabilities/CapabilityToken.java",
                "package net.minecraftforge.common.capabilities; public class CapabilityToken<T> {}\n",
            ),
            (
                "net/minecraftforge/common/capabilities/Capability.java",
                "package net.minecraftforge.common.capabilities; import net.minecraftforge.common.util.LazyOptional; public class Capability<T> { public boolean isRegistered() { return true; } public <R> LazyOptional<R> orEmpty(R value, Object requested) { return requested == this && value != null ? LazyOptional.ofNullable(value) : LazyOptional.empty(); } }\n",
            ),
            (
                "net/minecraftforge/common/capabilities/CapabilityManager.java",
                "package net.minecraftforge.common.capabilities; public final class CapabilityManager { private CapabilityManager() {} public static <T> Capability<T> get(CapabilityToken<T> token) { return new Capability<>(); } }\n",
            ),
            (
                "net/minecraftforge/common/capabilities/ICapabilityProvider.java",
                concat!(
                    "package net.minecraftforge.common.capabilities;\n",
                    "import net.minecraft.core.Direction;\n",
                    "import net.minecraftforge.common.util.LazyOptional;\n",
                    "public interface ICapabilityProvider {\n",
                    "    default <T> LazyOptional<T> getCapability(Capability<T> capability, Direction side) { return LazyOptional.empty(); }\n",
                    "    default <T> LazyOptional<T> getCapability(Capability<T> capability) { return getCapability(capability, null); }\n",
                    "}\n",
                ),
            ),
            (
                "net/neoforged/neoforge/capabilities/ICapabilityProvider.java",
                concat!(
                    "package net.neoforged.neoforge.capabilities;\n",
                    "import net.minecraft.core.Direction;\n",
                    "import net.neoforged.neoforge.common.util.LazyOptional;\n",
                    "public interface ICapabilityProvider {\n",
                    "    default <T> LazyOptional<T> getCapability(Capability<T> capability, Direction side) { return LazyOptional.empty(); }\n",
                    "    default <T> LazyOptional<T> getCapability(Capability<T> capability) { return getCapability(capability, null); }\n",
                    "}\n",
                ),
            ),
            (
                "net/minecraftforge/common/util/LazyOptional.java",
                concat!(
                    "package net.minecraftforge.common.util;\n",
                    "import java.util.Optional;\n",
                    "import java.util.function.Consumer;\n",
                    "import java.util.function.Function;\n",
                    "import java.util.function.Supplier;\n",
                    "public class LazyOptional<T> {\n",
                    "    private Supplier<? extends T> horizonSupplier;\n",
                    "    private T horizonValue;\n",
                    "    private boolean horizonValid = true;\n",
                    "    private LazyOptional(Supplier<? extends T> supplier) { this.horizonSupplier = supplier; }\n",
                    "    public static <T> LazyOptional<T> empty() { return new LazyOptional<>(() -> null); }\n",
                    "    public static <T> LazyOptional<T> of(Supplier<? extends T> supplier) { return new LazyOptional<>(supplier); }\n",
                    "    public static <T> LazyOptional<T> ofNullable(T value) { return new LazyOptional<>(() -> value); }\n",
                    "    private T horizonGet() { if (horizonValue == null && horizonSupplier != null) horizonValue = horizonSupplier.get(); return horizonValue; }\n",
                    "    public boolean isPresent() { return horizonValid && horizonGet() != null; }\n",
                    "    public T resolve() { return isPresent() ? horizonGet() : null; }\n",
                    "    public T orElse(T fallback) { T value = resolve(); return value == null ? fallback : value; }\n",
                    "    public T orElseGet(Supplier<? extends T> fallback) { T value = resolve(); return value == null ? fallback.get() : value; }\n",
                    "    public void ifPresent(Consumer<? super T> consumer) { T value = resolve(); if (value != null) consumer.accept(value); }\n",
                    "    public <R> LazyOptional<R> map(Function<? super T, ? extends R> mapper) { T value = resolve(); return value == null ? empty() : of(() -> mapper.apply(value)); }\n",
                    "    @SuppressWarnings(\"unchecked\") public <R> LazyOptional<R> cast() { return (LazyOptional<R>) this; }\n",
                    "    public void invalidate() { horizonValid = false; horizonValue = null; horizonSupplier = () -> null; }\n",
                    "    public LazyOptional<T> addListener(Consumer<LazyOptional<T>> listener) { if (!horizonValid) listener.accept(this); return this; }\n",
                    "    public Optional<T> resolveOptional() { return Optional.ofNullable(resolve()); }\n",
                    "}\n",
                ),
            ),
            (
                "net/neoforged/neoforge/items/IItemHandler.java",
                concat!(
                    "package net.neoforged.neoforge.items;\n",
                    "import net.minecraft.world.item.ItemStack;\n",
                    "import net.minecraft.world.item.Item;\n",
                    "public interface IItemHandler {\n",
                    // Keep a small, per-handler value store so capability
                    // bootstrap code can inspect and round-trip one bounded
                    // slot without pretending to be NeoForge's live block
                    // capability implementation. Reflection keeps this shim
                    // independent of which ItemStack members the census had
                    // to materialize in the generated game façade.
                    "    class HorizonSlot { ItemStack stack; int count; }\n",
                    "    java.util.Map<IItemHandler, HorizonSlot> HORIZON_SLOTS = java.util.Collections.synchronizedMap(new java.util.WeakHashMap<>());\n",
                    "    static HorizonSlot horizonSlot(IItemHandler self, int slot) { return slot == 0 ? HORIZON_SLOTS.computeIfAbsent(self, key -> new HorizonSlot()) : null; }\n",
                    "    static int horizonCount(ItemStack stack) { if (stack == null) return 0; try { return ((Integer) stack.getClass().getMethod(\"getCount\").invoke(stack)).intValue(); } catch (Exception ignored) { return 0; } }\n",
                    "    static Item horizonItem(ItemStack stack) { if (stack == null) return null; try { return (Item) stack.getClass().getMethod(\"getItem\").invoke(stack); } catch (Exception ignored) { return null; } }\n",
                    "    static ItemStack horizonCopy(ItemStack stack, int count) { if (stack == null || count <= 0) return new ItemStack(); try { return (ItemStack) stack.getClass().getMethod(\"copyWithCount\", int.class).invoke(stack, count); } catch (Exception ignored) { try { return new ItemStack(horizonItem(stack), count); } catch (Exception nested) { return new ItemStack(); } } }\n",
                    "    default int getSlots() { return 1; }\n",
                    "    default ItemStack getStackInSlot(int slot) { HorizonSlot value = horizonSlot(this, slot); return value == null || value.count <= 0 ? new ItemStack() : horizonCopy(value.stack, value.count); }\n",
                    "    default ItemStack insertItem(int slot, ItemStack stack, boolean simulate) { HorizonSlot value = horizonSlot(this, slot); int incoming = horizonCount(stack); if (value == null || incoming <= 0) return incoming <= 0 ? new ItemStack() : stack; Item existing = horizonItem(value.stack); Item incomingItem = horizonItem(stack); if (value.count > 0 && existing != incomingItem) return horizonCopy(stack, incoming); int amount = Math.min(incoming, Math.max(0, 64 - value.count)); if (!simulate && amount > 0) { if (value.count == 0) value.stack = stack; value.count += amount; } return amount == incoming ? new ItemStack() : horizonCopy(stack, incoming - amount); }\n",
                    "    default ItemStack extractItem(int slot, int amount, boolean simulate) { HorizonSlot value = horizonSlot(this, slot); if (value == null || amount <= 0 || value.count <= 0) return new ItemStack(); int moved = Math.min(amount, value.count); ItemStack result = horizonCopy(value.stack, moved); if (!simulate) { value.count -= moved; if (value.count == 0) value.stack = null; } return result; }\n",
                    "}\n",
                ),
            ),
            (
                "net/neoforged/neoforge/items/IItemHandlerModifiable.java",
                concat!(
                    "package net.neoforged.neoforge.items;\n",
                    "import net.minecraft.world.item.ItemStack;\n",
                    "public interface IItemHandlerModifiable extends IItemHandler {\n",
                    "    default void setStackInSlot(int slot, ItemStack stack) { IItemHandler.HorizonSlot value = IItemHandler.horizonSlot(this, slot); if (value != null) { value.stack = stack; value.count = IItemHandler.horizonCount(stack); } }\n",
                    "}\n",
                ),
            ),
            (
                "net/neoforged/fml/javafmlmod/FMLJavaModLoadingContext.java",
                concat!(
                    "package net.neoforged.fml.javafmlmod;\n",
                    "import net.neoforged.bus.api.IEventBus;\n",
                    "public class FMLJavaModLoadingContext {\n",
                    "    public static FMLJavaModLoadingContext get() { return new FMLJavaModLoadingContext(); }\n",
                    "    public IEventBus getModEventBus() { return new IEventBus() {}; }\n",
                    "}\n",
                ),
            ),
            (
                "net/minecraftforge/fml/javafmlmod/FMLJavaModLoadingContext.java",
                concat!(
                    "package net.minecraftforge.fml.javafmlmod;\n",
                    "import net.minecraftforge.eventbus.api.IEventBus;\n",
                    "public class FMLJavaModLoadingContext {\n",
                    "    public static FMLJavaModLoadingContext get() { return new FMLJavaModLoadingContext(); }\n",
                    "    public IEventBus getModEventBus() { return net.minecraftforge.common.MinecraftForge.EVENT_BUS; }\n",
                    "}\n",
                ),
            ),
            (
                "net/minecraftforge/fml/ModContainer.java",
                concat!(
                    "package net.minecraftforge.fml;\n",
                    "import net.minecraftforge.eventbus.api.IEventBus;\n",
                    "public class ModContainer {\n",
                    "    public IEventBus getEventBus() { return net.minecraftforge.common.MinecraftForge.EVENT_BUS; }\n",
                    "    public String getModId() { return \"\"; }\n",
                    "}\n",
                ),
            ),
            (
                "net/neoforged/neoforge/registries/RegisterEvent.java",
                register_event.as_str(),
            ),
            (
                "net/minecraft/core/Registry.java",
                concat!(
                    "package net.minecraft.core;\n",
                    "import java.util.*;\n",
                    "import net.minecraft.resources.*;\n",
                    "public class Registry<T> {\n",
                    "    protected final Map<Identifier,Object> horizonEntries = new LinkedHashMap<>();\n",
                    "    protected final Map<Identifier,ResourceKey> horizonKeys = new LinkedHashMap<>();\n",
                    "    public <V> V register(ResourceKey key, Identifier id, V value) { horizonEntries.put(id, value); horizonKeys.put(id, key); return value; }\n",
                    "    public boolean containsKey(Identifier id) { return horizonEntries.containsKey(id); }\n",
                    "    @SuppressWarnings(\"unchecked\") public T get(Identifier id) { return (T) horizonEntries.get(id); }\n",
                    "    public Optional<T> getOptional(Identifier id) { return Optional.ofNullable(get(id)); }\n",
                    "    public Identifier getKey(Object value) { for (Map.Entry<Identifier,Object> e : horizonEntries.entrySet()) if (Objects.equals(e.getValue(), value)) return e.getKey(); return null; }\n",
                    "    public Optional<ResourceKey> getResourceKey(Object value) { Identifier id = getKey(value); return id == null ? Optional.empty() : Optional.ofNullable(horizonKeys.get(id)); }\n",
                    "}\n",
                ),
            ),
            (
                "net/minecraft/core/DefaultedRegistry.java",
                defaulted_registry.as_str(),
            ),
            (
                "net/minecraft/resources/Identifier.java",
                concat!(
                    "package net.minecraft.resources;\n",
                    "public class Identifier {\n",
                    "    public Identifier() {}\n",
                    "    public Identifier(String namespace, String path) {}\n",
                    "}\n",
                ),
            ),
            (
                "net/minecraft/resources/ResourceKey.java",
                concat!(
                    "package net.minecraft.resources;\n",
                    "public class ResourceKey<T> { public ResourceKey() {} }\n",
                ),
            ),
            (
                "net/neoforged/neoforge/registries/DeferredHolder.java",
                concat!(
                    "package net.neoforged.neoforge.registries;\n",
                    "import java.util.function.Supplier;\n",
                    "import net.minecraft.resources.Identifier;\n",
                    "import net.minecraft.resources.ResourceKey;\n",
                    "public class DeferredHolder<T> {\n",
                    "    private T horizonValue;\n",
                    "    private Supplier<? extends T> horizonSupplier;\n",
                    "    private Identifier horizonId = new Identifier();\n",
                    "    private ResourceKey horizonKey = new ResourceKey();\n",
                    "    public DeferredHolder() {}\n",
                    "    DeferredHolder(Identifier id, ResourceKey key, Supplier<? extends T> supplier) { this.horizonId = id; this.horizonKey = key; this.horizonSupplier = supplier; }\n",
                    "    void horizonBind(Identifier id, ResourceKey key, Supplier<? extends T> supplier) { this.horizonId = id; this.horizonKey = key; this.horizonSupplier = supplier; }\n",
                    "    public T get() { if (horizonValue == null && horizonSupplier != null) horizonValue = horizonSupplier.get(); return horizonValue; }\n",
                    "    public T value() { return get(); }\n",
                    "    public boolean isPresent() { return get() != null; }\n",
                    "    public java.util.Optional<T> getOptional() { return java.util.Optional.ofNullable(get()); }\n",
                    "    public ResourceKey getKey() { return horizonKey; }\n",
                    "    public Identifier getId() { return horizonId; }\n",
                    "}\n",
                ),
            ),
            (
                "net/neoforged/neoforge/registries/DeferredRegister.java",
                concat!(
                    "package net.neoforged.neoforge.registries;\n",
                    "import java.util.function.Supplier;\n",
                    "import net.minecraft.resources.Identifier;\n",
                    "import net.minecraft.resources.ResourceKey;\n",
                    "import net.neoforged.bus.api.IEventBus;\n",
                    "public class DeferredRegister<T> {\n",
                    "    private String horizonNamespace = \"minecraft\";\n",
                    "    private Object horizonRegistry;\n",
                    "    public static <T> DeferredRegister<T> create(net.minecraft.core.Registry registry, String id) { DeferredRegister<T> value = new DeferredRegister<>(); value.horizonRegistry = registry; value.horizonNamespace = id; return value; }\n",
                    "    public static <T> DeferredRegister<T> create(net.minecraft.resources.ResourceKey registryKey, String id) { DeferredRegister<T> value = new DeferredRegister<>(); value.horizonNamespace = id; return value; }\n",
                    "    public <V extends T> DeferredHolder<V> register(String id, Supplier<V> value) { DeferredHolder<V> holder = new DeferredHolder<>(); holder.horizonBind(new Identifier(horizonNamespace, id), new ResourceKey(), value); return holder; }\n",
                    "    public void register(IEventBus bus) {}\n",
                    "}\n",
                ),
            ),
            (
                "net/neoforged/neoforge/network/registration/PayloadRegistrar.java",
                NEOFORGE_PAYLOAD_REGISTRAR,
            ),
            (
                "net/neoforged/neoforge/network/handling/IPayloadContext.java",
                "package net.neoforged.neoforge.network.handling;\npublic interface IPayloadContext {}\n",
            ),
            (
                "net/neoforged/neoforge/network/handling/IPayloadHandler.java",
                "package net.neoforged.neoforge.network.handling;\n@FunctionalInterface public interface IPayloadHandler<T> { void handle(T payload, IPayloadContext context); }\n",
            ),
            (
                "net/neoforged/neoforge/network/event/RegisterPayloadHandlersEvent.java",
                concat!(
                    "package net.neoforged.neoforge.network.event;\n",
                    "import net.neoforged.bus.api.Event;\n",
                    "import net.neoforged.neoforge.network.registration.PayloadRegistrar;\n",
                    "public class RegisterPayloadHandlersEvent extends Event {\n",
                    "    private final PayloadRegistrar horizonRegistrar = new PayloadRegistrar();\n",
                    "    public PayloadRegistrar registrar(String version) { return horizonRegistrar; }\n",
                    "}\n",
                ),
            ),
            (
                "net/neoforged/neoforge/capabilities/Capability.java",
                "package net.neoforged.neoforge.capabilities;\npublic class Capability<T> {}\n",
            ),
            (
                "net/neoforged/neoforge/capabilities/BlockCapability.java",
                "package net.neoforged.neoforge.capabilities;\npublic class BlockCapability<T, C> extends Capability<T> {}\n",
            ),
            (
                "net/neoforged/neoforge/capabilities/ItemCapability.java",
                "package net.neoforged.neoforge.capabilities;\npublic class ItemCapability<T, C> extends Capability<T> {}\n",
            ),
            (
                "net/neoforged/neoforge/capabilities/EntityCapability.java",
                "package net.neoforged.neoforge.capabilities;\npublic class EntityCapability<T, C> extends Capability<T> {}\n",
            ),
            (
                "net/neoforged/neoforge/capabilities/BlockCapabilityCache.java",
                "package net.neoforged.neoforge.capabilities;\npublic final class BlockCapabilityCache { private BlockCapabilityCache() {} @FunctionalInterface public interface Provider<T, C> { T getCapability(net.minecraft.world.level.Level level, net.minecraft.core.BlockPos pos, net.minecraft.world.level.block.state.BlockState state, net.minecraft.world.level.block.entity.BlockEntity blockEntity, C context); } }\n",
            ),
            (
                "net/neoforged/neoforge/capabilities/ItemCapabilityCache.java",
                "package net.neoforged.neoforge.capabilities;\npublic final class ItemCapabilityCache { private ItemCapabilityCache() {} @FunctionalInterface public interface Provider<T, C> { T getCapability(net.minecraft.world.item.ItemStack stack, C context); } }\n",
            ),
            (
                "net/neoforged/neoforge/capabilities/EntityCapabilityCache.java",
                "package net.neoforged.neoforge.capabilities;\npublic final class EntityCapabilityCache { private EntityCapabilityCache() {} @FunctionalInterface public interface Provider<T, C> { T getCapability(net.minecraft.world.entity.Entity entity, C context); } }\n",
            ),
            (
                "net/neoforged/neoforge/transfer/ResourceHandler.java",
                "package net.neoforged.neoforge.transfer;\npublic interface ResourceHandler<R> { int size(); R getResource(); long getAmount(); long getCapacity(); }\n",
            ),
            (
                "net/minecraftforge/network/simple/SimpleChannel.java",
                LEGACY_FORGE_SIMPLE_CHANNEL_SOURCE,
            ),
            (
                "net/minecraftforge/network/NetworkRegistry.java",
                LEGACY_FORGE_NETWORK_REGISTRY_SOURCE,
            ),
        ];
        for (relative, source) in shims {
            // A shim whose shape the game's own stub already provides (see
            // `defaulted_registry`) contributes no file.
            if source.is_empty() {
                continue;
            }
            if !uses_neoforge_entity_events
                && (relative.starts_with("net/neoforged/neoforge/capabilities/")
                    || relative == "net/neoforged/neoforge/transfer/ResourceHandler.java")
            {
                continue;
            }
            if relative == format!("{NEOFORGE_REGISTER_CAPABILITIES_EVENT}.java")
                && ![
                    "net/minecraft/world/level/Level",
                    "net/minecraft/core/BlockPos",
                    "net/minecraft/world/level/block/Block",
                    "net/minecraft/world/level/block/state/BlockState",
                    "net/minecraft/world/level/block/entity/BlockEntity",
                    "net/minecraft/world/level/block/entity/BlockEntityType",
                    "net/minecraft/world/entity/Entity",
                    "net/minecraft/world/entity/EntityType",
                ]
                .iter()
                .all(|name| available.contains(*name))
            {
                report.missing.push(format!(
                    "{NEOFORGE_REGISTER_CAPABILITIES_EVENT} (needs capability provider game types)"
                ));
                continue;
            }
            let game_value = match relative {
                "net/minecraft/core/Registry.java" => Some("net/minecraft/core/Registry"),
                "net/minecraft/core/DefaultedRegistry.java" => {
                    Some("net/minecraft/core/DefaultedRegistry")
                }
                "net/minecraft/resources/Identifier.java" => {
                    Some("net/minecraft/resources/Identifier")
                }
                "net/minecraft/resources/ResourceKey.java" => {
                    Some("net/minecraft/resources/ResourceKey")
                }
                _ => None,
            };
            if game_value.is_some_and(|name| wanted.contains(name) || lib_names.contains(name)) {
                continue;
            }
            let file = src.join(relative);
            if let Some(parent) = file.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut source = source;
            if relative == format!("{NEOFORGE_BIOME_MODIFIER}.java")
                && ![
                    "net/minecraft/core/RegistryAccess",
                    "net/minecraft/core/Holder",
                    "net/minecraft/world/level/biome/Biome",
                    "net/minecraft/world/level/biome/BiomeGenerationSettings",
                ]
                .iter()
                .all(|name| game.contains_key(*name))
            {
                source = NEOFORGE_BIOME_MODIFIER_OBJECT_SOURCE;
                report.bound_legacy(
                    NEOFORGE_BIOME_MODIFIER,
                    "modify/codec",
                    "game jar lacks BiomeModifier's typed RegistryAccess/Holder/Biome/BiomeGenerationSettings",
                );
            }
            fs::write(&file, source).map_err(|e| format!("{}: {e}", file.display()))?;
            files.push(file);
            // Legacy Forge's channel factory is the one shim in this array that
            // cannot carry its real descriptor; name it in the report.
            if relative == "net/minecraftforge/network/NetworkRegistry.java" {
                report.bound_legacy(
                    "net/minecraftforge/network/NetworkRegistry",
                    "newSimpleChannel",
                    "legacy Forge names ResourceLocation/NetworkProtocolVersion, absent from 26.3",
                );
            }
            if relative == format!("{NEOFORGE_PAYLOAD_REGISTRAR_CLASS}.java") {
                report.bound_legacy(
                    NEOFORGE_PAYLOAD_REGISTRAR_CLASS,
                    "play*/configuration*",
                    "legacy Object overloads keep a payload type outside CustomPacketPayload linking; the typed overloads serve the real descriptor",
                );
            }
        }
        // NeoForge's client bootstrap surface: key-mapping registration keeps
        // the mappings it was handed, and the tick/render events stay markers
        // the host posts. Same availability guard as the Fabric slice, so a
        // real NeoForge jar is never shadowed.
        if uses_neoforge_client {
            let key_mappings = "net/neoforged/neoforge/client/event/RegisterKeyMappingsEvent";
            client_shims.emit(
                &mut files,
                &mut report,
                key_mappings,
                &["key_mapping"],
                || fill_client(NEOFORGE_REGISTER_KEY_MAPPINGS),
            )?;
            let tick = "net/neoforged/neoforge/client/event/ClientTickEvent";
            client_shims.emit(&mut files, &mut report, tick, &[], || {
                NEOFORGE_CLIENT_TICK_EVENT.to_owned()
            })?;
            let render = "net/neoforged/neoforge/client/event/RenderGuiEvent";
            client_shims.emit(&mut files, &mut report, render, &["gui"], || {
                fill_client(NEOFORGE_RENDER_GUI_EVENT)
            })?;
        }
        // NeoForge's screen events: `Init` and `Render` and their `Pre`/`Post`
        // phases are markers the host posts, each carrying its screen.
        if uses_neoforge_screens {
            client_shims.emit(
                &mut files,
                &mut report,
                NEOFORGE_SCREEN_EVENT,
                &["screen"],
                || fill_client(NEOFORGE_SCREEN_EVENT_SOURCE),
            )?;
        }
        // NeoForge's registration events: the creative-tab contents event keeps
        // the stacks a listener accepts, and the renderer registration event
        // keeps the pairs it is handed. Both stay bounded values the host posts.
        if uses_neoforge_registration {
            client_shims.emit(
                &mut files,
                &mut report,
                "net/neoforged/neoforge/event/BuildCreativeModeTabContentsEvent",
                &["creative_mode_tab", "item_stack"],
                || fill_client(NEOFORGE_BUILD_CREATIVE_MODE_TAB_CONTENTS),
            )?;
            client_shims.emit(
                &mut files,
                &mut report,
                "net/neoforged/neoforge/client/event/EntityRenderersEvent",
                &[
                    "block_entity_renderer_provider",
                    "block_entity_type",
                    "entity_renderer_provider",
                    "entity_type",
                ],
                || fill_client(NEOFORGE_ENTITY_RENDERERS),
            )?;
        }
        if uses_neoforge_entity_events {
            for (class, source) in [
                (
                    NEOFORGE_ENTITY_JOIN_LEVEL_EVENT,
                    NEOFORGE_ENTITY_JOIN_LEVEL_EVENT_SOURCE,
                ),
                (
                    NEOFORGE_REGISTER_CAPABILITIES_EVENT,
                    NEOFORGE_REGISTER_CAPABILITIES_EVENT_SOURCE,
                ),
                (NEOFORGE_BIOME_MODIFIER, NEOFORGE_BIOME_MODIFIER_SOURCE),
                (NEOFORGE_ISTORAGE, NEOFORGE_ISTORAGE_SOURCE),
            ] {
                if available.contains(class) {
                    continue;
                }
                // The join-level event's 26.3 shape names `Entity` and `Level`
                // in its signatures; a jar that lacks either cannot compile it
                // and is reported instead of getting an Object-shaped copy.
                if class == NEOFORGE_ENTITY_JOIN_LEVEL_EVENT
                    && !(game.contains_key("net/minecraft/world/entity/Entity")
                        && game.contains_key("net/minecraft/world/level/Level"))
                {
                    report.missing.push(format!(
                        "{class} (needs net/minecraft/world/entity/Entity, net/minecraft/world/level/Level)"
                    ));
                    continue;
                }
                // The biome modifier's typed shape names four game types; a
                // partial game jar keeps the Object-shaped copy instead.
                let mut source = source;
                if class == NEOFORGE_BIOME_MODIFIER
                    && ![
                        "net/minecraft/core/RegistryAccess",
                        "net/minecraft/core/Holder",
                        "net/minecraft/world/level/biome/Biome",
                        "net/minecraft/world/level/biome/BiomeGenerationSettings",
                    ]
                    .iter()
                    .all(|name| game.contains_key(*name))
                {
                    source = NEOFORGE_BIOME_MODIFIER_OBJECT_SOURCE;
                    report.bound_legacy(
                        class,
                        "modify/codec",
                        "game jar lacks BiomeModifier's typed RegistryAccess/Holder/Biome/BiomeGenerationSettings",
                    );
                }
                if class == NEOFORGE_ISTORAGE {
                    report.bound_legacy(
                        class,
                        "readNBT/writeNBT",
                        "IStorage is a legacy marker; its Capability/Direction/CompoundTag descriptors are not 26.3 classes",
                    );
                }
                write_shim(&mut files, &src, class, source)?;
            }
        }
        // NeoForge's model events: `RegisterAdditional` retains the model keys
        // a listener asks for and `ModifyBakingResult` the baked models it hands
        // back, both in call order. The host owns baking, so these stay bounded
        // values it posts, never a live model registry.
        if uses_neoforge_models {
            client_shims.emit(
                &mut files,
                &mut report,
                NEOFORGE_MODEL_EVENT,
                &["model_key", "baked_model"],
                || fill_client(NEOFORGE_MODEL_EVENT_SOURCE),
            )?;
        }
    }
    // NeoForge's attachment type: the builder keeps the codec and default
    // markers plus the copy-on-death flag, and the registration marker retains
    // the id it is handed. A real NeoForge jar is never shadowed.
    if uses_neoforge_attachments {
        emit_role_shim(
            &mut files,
            &mut report,
            &src,
            &available,
            &attachment_types,
            NEOFORGE_ATTACHMENT_TYPE,
            &["identifier", "codec"],
            || fill_types(NEOFORGE_ATTACHMENT_TYPE_SOURCE, &attachment_types),
        )?;
    }
    // Fabric's menu type and factory. The class name and package are roles here
    // because 26.3 renamed the pair, and a jar still built against the 1.20.x
    // names must bind too: a pair lands only for the name the target names.
    if uses_fabric_menus {
        for (class, requires, template) in [
            (
                FABRIC_MENU_TYPE,
                &["menu", "menu_type", "inventory", "codec"][..],
                FABRIC_MENU_TYPE_SOURCE,
            ),
            (
                FABRIC_SCREEN_HANDLER_TYPE,
                &["menu", "menu_type", "inventory", "codec"][..],
                FABRIC_MENU_TYPE_SOURCE,
            ),
            (
                FABRIC_MENU_FACTORY,
                &[
                    "menu_provider",
                    "server_player",
                    "component",
                    "screen",
                    "inventory",
                ][..],
                FABRIC_MENU_FACTORY_SOURCE,
            ),
            (
                FABRIC_SCREEN_HANDLER_FACTORY,
                &[
                    "menu_provider",
                    "server_player",
                    "component",
                    "screen",
                    "inventory",
                ][..],
                FABRIC_MENU_FACTORY_SOURCE,
            ),
        ] {
            if !names_any(&[class]) {
                continue;
            }
            let (package, simple) = class.rsplit_once('/').unwrap_or(("", class));
            let mut types = menu_types.clone();
            types.insert("package", package.to_owned());
            types.insert("name", simple.to_owned());
            emit_role_shim(
                &mut files,
                &mut report,
                &src,
                &available,
                &types,
                class,
                requires,
                || fill_types(template, &types),
            )?;
        }
    }
    // NeoForge's menu factories and its `MenuType` extension. The extension
    // names the factory, so the factory lands whenever the slice does; each is
    // still guarded on the real API class being absent.
    if uses_neoforge_menus {
        emit_role_shim(
            &mut files,
            &mut report,
            &src,
            &available,
            &menu_types,
            NEOFORGE_CONTAINER_FACTORY,
            &["menu", "inventory", "byte_buf"],
            || fill_types(NEOFORGE_CONTAINER_FACTORY_SOURCE, &menu_types),
        )?;
        if names_any(&[NEOFORGE_EXTENDED_CONTAINER_FACTORY]) {
            emit_role_shim(
                &mut files,
                &mut report,
                &src,
                &available,
                &menu_types,
                NEOFORGE_EXTENDED_CONTAINER_FACTORY,
                &["menu"],
                || fill_types(NEOFORGE_EXTENDED_CONTAINER_FACTORY_SOURCE, &menu_types),
            )?;
        }
        if names_any(&[NEOFORGE_MENU_TYPE_EXTENSION]) {
            emit_role_shim(
                &mut files,
                &mut report,
                &src,
                &available,
                &menu_types,
                NEOFORGE_MENU_TYPE_EXTENSION,
                &["menu", "menu_type", "identifier"],
                || fill_types(NEOFORGE_MENU_TYPE_EXTENSION_SOURCE, &menu_types),
            )?;
        }
    }
    if files.is_empty() {
        return Err("nothing to generate".to_owned());
    }
    let list = out.join("sources.txt");
    let mut args = String::new();
    for file in &files {
        let _ = writeln!(args, "\"{}\"", file.display());
    }
    fs::write(&list, args).map_err(|e| e.to_string())?;
    // `classes::read_jar` sees Fabric's nested API jars for shape discovery,
    // but javac cannot read `META-INF/jars/` itself. Extract those dependencies
    // into a bounded scratch classpath for this compilation, including nested
    // jars inside a bundled jar.
    let nested = extract_nested_jars(
        library_jars.iter().chain(mod_jars.iter()),
        &out.join(".nested-classpath"),
    )?;
    let mut compile_jars = library_jars.to_vec();
    compile_jars.extend(nested.jars.iter().cloned());
    for path in mod_jars {
        if !compile_jars.iter().any(|existing| existing == path) {
            compile_jars.push(path.clone());
        }
    }
    let mut command = Command::new(javac);
    command
        .arg("-d")
        .arg(&classes_dir)
        .arg("-proc:none")
        .arg("-nowarn");
    if !compile_jars.is_empty() {
        let path = std::env::join_paths(&compile_jars).map_err(|e| e.to_string())?;
        command.arg("-classpath").arg(path);
    }
    let output = command
        .arg(format!("@{}", list.display()))
        .output()
        .map_err(|e| format!("{}: {e}", javac.display()))?;
    if !output.status.success() {
        // `javac` diagnoses the exact file and line; a caller (the launcher or
        // a test) has nothing else to go on, so keep them in the error instead
        // of letting them drain to a terminal nobody reads.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let diagnostics = format!(
            "{}{}{}",
            stdout.trim(),
            if stdout.trim().is_empty() { "" } else { "\n" },
            stderr.trim()
        );
        return Err(if diagnostics.is_empty() {
            format!("javac failed: {}", output.status)
        } else {
            format!("javac failed: {}\n{diagnostics}", output.status)
        });
    }
    let output = Command::new(jar)
        .arg("--create")
        .arg("--file")
        .arg(&report.jar)
        .arg("-C")
        .arg(&classes_dir)
        .arg(".")
        .output()
        .map_err(|e| format!("{}: {e}", jar.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("jar failed: {}: {}", output.status, stderr.trim()));
    }
    Ok(report)
}

struct NestedClasspath {
    root: PathBuf,
    jars: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct MixinImpl {
    interface: String,
    methods: Vec<Declared>,
}

impl Drop for NestedClasspath {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Extract direct `META-INF/jars/*.jar` entries recursively for javac. The
/// runtime runner has the same bound; keeping it here prevents a malformed
/// mod from turning façade generation into an unbounded archive walk.
fn extract_nested_jars<'a>(
    inputs: impl IntoIterator<Item = &'a PathBuf>,
    root: &Path,
) -> Result<NestedClasspath, String> {
    const MAX_NESTED_JARS: usize = 4096;
    const MAX_NESTED_SIZE: u64 = 32 * 1024 * 1024;
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).map_err(|e| format!("{}: {e}", root.display()))?;
    let mut queue: Vec<PathBuf> = inputs.into_iter().cloned().collect();
    let mut jars = Vec::new();
    let mut serial = 0usize;
    let mut index = 0usize;
    while index < queue.len() {
        let input = queue[index].clone();
        index += 1;
        let Ok(file) = fs::File::open(&input) else {
            continue;
        };
        let Ok(mut archive) = zip::ZipArchive::new(file) else {
            continue;
        };
        for entry_index in 0..archive.len() {
            let mut entry = archive
                .by_index(entry_index)
                .map_err(|e| format!("{}: reading zip entry: {e}", input.display()))?;
            let Some(name) = nested_jar_name(entry.name()) else {
                continue;
            };
            if entry.size() > MAX_NESTED_SIZE {
                return Err(format!(
                    "{}: nested jar {name} exceeds {MAX_NESTED_SIZE} bytes",
                    input.display()
                ));
            }
            if serial >= MAX_NESTED_JARS {
                return Err(format!(
                    "nested jar limit exceeded ({MAX_NESTED_JARS}); refusing an unbounded classpath"
                ));
            }
            let output = root.join(format!("{serial}-{name}"));
            serial += 1;
            let mut out =
                fs::File::create(&output).map_err(|e| format!("{}: {e}", output.display()))?;
            io::copy(&mut entry, &mut out).map_err(|e| format!("{}: {e}", output.display()))?;
            jars.push(output.clone());
            queue.push(output);
        }
    }
    Ok(NestedClasspath {
        root: root.to_path_buf(),
        jars,
    })
}

fn nested_jar_name(entry: &str) -> Option<&str> {
    let name = entry.strip_prefix("META-INF/jars/")?;
    if name.is_empty() || name.contains('/') || !name.ends_with(".jar") {
        return None;
    }
    Some(name)
}

/// The generated `FriendlyByteBuf`'s wire state: a bounded, growable
/// big-endian byte buffer with a read cursor, plus the `horizonValues` marker
/// FIFO that keeps opaque values (NBT tags) without a wire shape.
///
/// Every allocation is bounded by the `HORIZON_MAX_BYTES` (1 MiB) constant,
/// the same limit Fabric's payload accessor advertises. Reads past the end
/// answer the primitive zero (or `""`) rather than throwing, because a façade
/// buffer is also handed to decoders the game fills elsewhere; only a declared
/// length that cannot be valid throws.
const FRIENDLY_BYTE_BUF_STATE: &str = r#"    protected java.util.List<Object> horizonValues = new java.util.ArrayList<>();
    protected int horizonReadIndex;
    protected byte[] horizonBytes = new byte[0];
    protected int horizonWriteIndex;
    protected int horizonByteReadIndex;
    // Index views never grow the bounded buffer: requested positions are clamped
    // to the existing written window, preserving the façade's zero-copy shape.
    protected static final int HORIZON_MAX_BYTES = 1048576;
    protected void horizonEnsure(int extra) {
        if (extra < 0 || extra > HORIZON_MAX_BYTES) throw new IllegalArgumentException("Invalid payload length: " + extra);
        int needed = horizonWriteIndex + extra;
        if (needed > HORIZON_MAX_BYTES) throw new IllegalArgumentException("Payload exceeds " + HORIZON_MAX_BYTES + " bytes");
        if (needed > horizonBytes.length) {
            int grown = Math.max(needed, Math.max(64, horizonBytes.length * 2));
            horizonBytes = java.util.Arrays.copyOf(horizonBytes, Math.min(grown, HORIZON_MAX_BYTES));
        }
    }
    protected void horizonPut(int value) {
        horizonEnsure(1);
        horizonBytes[horizonWriteIndex++] = (byte) value;
    }
    protected void horizonPutBytes(byte[] value) {
        if (value == null || value.length == 0) return;
        horizonEnsure(value.length);
        System.arraycopy(value, 0, horizonBytes, horizonWriteIndex, value.length);
        horizonWriteIndex += value.length;
    }
    protected int horizonGet() {
        if (horizonByteReadIndex >= horizonWriteIndex) return 0;
        return horizonBytes[horizonByteReadIndex++] & 255;
    }
    protected int horizonReadable() {
        return Math.max(0, horizonWriteIndex - horizonByteReadIndex);
    }
    protected void horizonSkip(int count) {
        horizonByteReadIndex = Math.min(horizonWriteIndex, horizonByteReadIndex + Math.max(0, count));
    }
    protected void horizonCompact() {
        int readable = horizonReadable();
        if (horizonByteReadIndex > 0 && readable > 0) System.arraycopy(horizonBytes, horizonByteReadIndex, horizonBytes, 0, readable);
        horizonWriteIndex = readable;
        horizonByteReadIndex = 0;
    }
    protected void horizonSetWriterIndex(int index) {
        int limit = Math.max(0, Math.min(index, HORIZON_MAX_BYTES));
        if (limit > horizonWriteIndex) {
            horizonEnsure(limit - horizonWriteIndex);
            java.util.Arrays.fill(horizonBytes, horizonWriteIndex, limit, (byte) 0);
        }
        horizonWriteIndex = limit;
        if (horizonByteReadIndex > limit) horizonByteReadIndex = limit;
    }
    protected int horizonSetReaderIndexView(int index) {
        int previous = horizonByteReadIndex;
        horizonByteReadIndex = Math.max(0, Math.min(index, horizonWriteIndex));
        return previous;
    }
    protected int horizonSetWriterIndexView(int index) {
        int previous = horizonWriteIndex;
        int limit = Math.max(0, Math.min(index, horizonWriteIndex));
        horizonWriteIndex = limit;
        if (horizonByteReadIndex > limit) horizonByteReadIndex = limit;
        return previous;
    }
    protected void horizonSetIndexView(int index) {
        int limit = Math.max(0, Math.min(index, horizonWriteIndex));
        horizonByteReadIndex = limit;
        horizonWriteIndex = limit;
    }
    protected void horizonPutShort(int value) {
        horizonPut(value >> 8);
        horizonPut(value);
    }
    protected void horizonPutShortLE(int value) {
        horizonPut(value);
        horizonPut(value >> 8);
    }
    protected int horizonGetShort() {
        return (horizonGet() << 8) | horizonGet();
    }
    protected int horizonGetShortLE() {
        return horizonGet() | (horizonGet() << 8);
    }
    protected void horizonPutMedium(int value) {
        horizonPut(value >> 16);
        horizonPut(value >> 8);
        horizonPut(value);
    }
    protected void horizonPutMediumLE(int value) {
        horizonPut(value);
        horizonPut(value >> 8);
        horizonPut(value >> 16);
    }
    protected int horizonGetMedium() {
        int value = (horizonGet() << 16) | (horizonGet() << 8) | horizonGet();
        return (value & 0x800000) == 0 ? value : value | 0xff000000;
    }
    protected int horizonGetMediumLE() {
        int value = horizonGet() | (horizonGet() << 8) | (horizonGet() << 16);
        return (value & 0x800000) == 0 ? value : value | 0xff000000;
    }
    protected void horizonPutInt(int value) {
        horizonPut(value >> 24);
        horizonPut(value >> 16);
        horizonPut(value >> 8);
        horizonPut(value);
    }
    protected void horizonPutIntLE(int value) {
        horizonPut(value);
        horizonPut(value >> 8);
        horizonPut(value >> 16);
        horizonPut(value >> 24);
    }
    protected int horizonGetInt() {
        return (horizonGet() << 24) | (horizonGet() << 16) | (horizonGet() << 8) | horizonGet();
    }
    protected int horizonGetIntLE() {
        return horizonGet() | (horizonGet() << 8) | (horizonGet() << 16) | (horizonGet() << 24);
    }
    protected void horizonPutLong(long value) {
        for (int shift = 56; shift >= 0; shift -= 8) horizonPut((int) (value >> shift));
    }
    protected void horizonPutLongLE(long value) {
        for (int shift = 0; shift <= 56; shift += 8) horizonPut((int) (value >> shift));
    }
    protected long horizonGetLong() {
        long value = 0L;
        for (int i = 0; i < 8; i++) value = (value << 8) | (long) horizonGet();
        return value;
    }
    protected long horizonGetLongLE() {
        long value = 0L;
        for (int shift = 0; shift <= 56; shift += 8) value |= (long) horizonGet() << shift;
        return value;
    }
    protected void horizonPutVarInt(int value) {
        int remaining = value;
        while ((remaining & -128) != 0) {
            horizonPut((remaining & 127) | 128);
            remaining >>>= 7;
        }
        horizonPut(remaining);
    }
    protected int horizonGetVarInt() {
        int value = 0;
        for (int shift = 0; shift < 35; shift += 7) {
            int next = horizonGet();
            value |= (next & 127) << shift;
            if ((next & 128) == 0) return value;
        }
        return value;
    }
    protected void horizonPutVarLong(long value) {
        long remaining = value;
        while ((remaining & -128L) != 0L) {
            horizonPut((int) (remaining & 127L) | 128);
            remaining >>>= 7;
        }
        horizonPut((int) remaining);
    }
    protected long horizonGetVarLong() {
        long value = 0L;
        for (int shift = 0; shift < 70; shift += 7) {
            int next = horizonGet();
            value |= (long) (next & 127) << shift;
            if ((next & 128) == 0) return value;
        }
        return value;
    }
    protected void horizonPutUtf(String value, int maxLength) {
        int limit = (int) Math.min((long) Math.max(0, maxLength) * 3L, (long) HORIZON_MAX_BYTES);
        byte[] data = (value == null ? "" : value).getBytes(java.nio.charset.StandardCharsets.UTF_8);
        if (data.length > limit) throw new IllegalArgumentException("String too big (was " + data.length + " bytes encoded, max " + limit + ")");
        horizonPutVarInt(data.length);
        horizonPutBytes(data);
    }
    protected String horizonGetUtf(int maxLength) {
        int limit = (int) Math.min((long) Math.max(0, maxLength) * 3L, (long) HORIZON_MAX_BYTES);
        int length = horizonGetVarInt();
        if (length < 0 || length > limit) throw new IllegalArgumentException("Invalid string length: " + length);
        if (length > horizonReadable()) return "";
        byte[] data = new byte[length];
        for (int i = 0; i < length; i++) data[i] = (byte) horizonGet();
        return new String(data, java.nio.charset.StandardCharsets.UTF_8);
    }
    protected byte[] horizonGetByteArray(int maxLength) {
        int limit = maxLength < 0 ? HORIZON_MAX_BYTES : Math.min(maxLength, HORIZON_MAX_BYTES);
        int length = horizonGetVarInt();
        if (length < 0 || length > limit) throw new IllegalArgumentException("Invalid byte array length: " + length);
        if (length > horizonReadable()) return new byte[0];
        byte[] data = new byte[length];
        for (int i = 0; i < length; i++) data[i] = (byte) horizonGet();
        return data;
    }
    protected int horizonGetArrayLength() {
        int length = horizonGetVarInt();
        if (length < 0 || length > HORIZON_MAX_BYTES) throw new IllegalArgumentException("Invalid array length: " + length);
        return length;
    }
    protected void horizonPutBooleanArray(boolean[] value) {
        int length = value == null ? 0 : value.length;
        horizonPutVarInt(length);
        if (value != null) for (boolean item : value) horizonPut(item ? 1 : 0);
    }
    protected boolean[] horizonGetBooleanArray() {
        int length = horizonGetArrayLength();
        if (length > horizonReadable()) return new boolean[0];
        boolean[] value = new boolean[length];
        for (int i = 0; i < length; i++) value[i] = horizonGet() != 0;
        return value;
    }
    protected void horizonPutIntArray(int[] value) {
        int length = value == null ? 0 : value.length;
        horizonPutVarInt(length);
        if (value != null) for (int item : value) horizonPutInt(item);
    }
    protected int[] horizonGetIntArray() {
        int length = horizonGetArrayLength();
        if (length > horizonReadable() / 4) return new int[0];
        int[] value = new int[length];
        for (int i = 0; i < length; i++) value[i] = horizonGetInt();
        return value;
    }
    protected void horizonPutLongArray(long[] value) {
        int length = value == null ? 0 : value.length;
        horizonPutVarInt(length);
        if (value != null) for (long item : value) horizonPutLong(item);
    }
    protected long[] horizonGetLongArray() {
        int length = horizonGetArrayLength();
        if (length > horizonReadable() / 8) return new long[0];
        long[] value = new long[length];
        for (int i = 0; i < length; i++) value[i] = horizonGetLong();
        return value;
    }
    protected void horizonPutFloatArray(float[] value) {
        int length = value == null ? 0 : value.length;
        horizonPutVarInt(length);
        if (value != null) for (float item : value) horizonPutInt(Float.floatToRawIntBits(item));
    }
    protected float[] horizonGetFloatArray() {
        int length = horizonGetArrayLength();
        if (length > horizonReadable() / 4) return new float[0];
        float[] value = new float[length];
        for (int i = 0; i < length; i++) value[i] = Float.intBitsToFloat(horizonGetInt());
        return value;
    }
    protected void horizonPutDoubleArray(double[] value) {
        int length = value == null ? 0 : value.length;
        horizonPutVarInt(length);
        if (value != null) for (double item : value) horizonPutLong(Double.doubleToRawLongBits(item));
    }
    protected double[] horizonGetDoubleArray() {
        int length = horizonGetArrayLength();
        if (length > horizonReadable() / 8) return new double[0];
        double[] value = new double[length];
        for (int i = 0; i < length; i++) value[i] = Double.longBitsToDouble(horizonGetLong());
        return value;
    }
    protected void horizonPutStringArray(String[] value) {
        int length = value == null ? 0 : value.length;
        horizonPutVarInt(length);
        if (value != null) for (String item : value) horizonPutUtf(item, 32767);
    }
    protected String[] horizonGetStringArray() {
        int length = horizonGetArrayLength();
        if (length > horizonReadable()) return new String[0];
        String[] value = new String[length];
        for (int i = 0; i < length; i++) value[i] = horizonGetUtf(32767);
        return value;
    }
"#;

/// The generated `StreamCodec`'s wire state: the value a codec carries and the
/// callbacks Vanilla's factories (`of`, `unit`, `map`, `dispatch`, `recursive`,
/// `composite`) keep, so `encode`/`decode` really walk the shape the mod built.
///
/// `horizonKind` says which factory built the codec: 0 an `of` or a mod's own
/// anonymous codec (or a `unit`, when `horizonValue` is set), 1 `map`, 2
/// `dispatch`, 3 `recursive`, 4 `composite`. A composite's members and their
/// getters each have their own field, because the encode and decode halves
/// visit them in wire order.
///
/// The bodies live in `stream_codec_body` and, for the two shared halves,
/// `ENCODE_CODEC`/`DECODE_CODEC`; every one of them calls into these helpers.
const STREAM_CODEC_STATE: &str = r#"    private static final int HORIZON_MAP = 1;
    private static final int HORIZON_DISPATCH = 2;
    private static final int HORIZON_RECURSIVE = 3;
    private static final int HORIZON_COMPOSITE = 4;
    protected Object horizonValue;
    protected int horizonKind;
    protected java.util.function.Function horizonFirst;
    protected java.util.function.Function horizonSecond;
    protected Object horizonConstructor;
    protected java.util.function.Function horizonGetters1;
    protected java.util.function.Function horizonGetters2;
    protected java.util.function.Function horizonGetters3;
    protected java.util.function.Function horizonGetters4;
    protected java.util.function.Function horizonGetters5;
    protected java.util.function.Function horizonGetters6;
    protected java.util.function.Function horizonGetters7;
    protected java.util.function.Function horizonGetters8;
    protected net.minecraft.network.codec.StreamCodec horizonMember1;
    protected net.minecraft.network.codec.StreamCodec horizonMember2;
    protected net.minecraft.network.codec.StreamCodec horizonMember3;
    protected net.minecraft.network.codec.StreamCodec horizonMember4;
    protected net.minecraft.network.codec.StreamCodec horizonMember5;
    protected net.minecraft.network.codec.StreamCodec horizonMember6;
    protected net.minecraft.network.codec.StreamCodec horizonMember7;
    protected net.minecraft.network.codec.StreamCodec horizonMember8;
    protected net.minecraft.network.codec.StreamCodec horizonRecursiveCodec;
    /** The `StreamCodec.of` pair: the encoder is a `Function`-shaped
     *  `StreamEncoder` (its buffer pair travels as one argument), the decoder
     *  is a `StreamDecoder` with the buffer as its only one. A `unit` codec
     *  keeps `horizonValue`; an `of` codec never sets it, since its value
     *  comes from the decoder. */
    protected net.minecraft.network.codec.StreamCodec horizonOf(Object encoder, Object decoder) {
        horizonFirst = horizonEncoder(encoder);
        horizonSecond = horizonDecoder(decoder);
        return this;
    }
    /** The encoder `of` was handed, as a one-argument call: the buffer pair
     *  travels in an `Object[]`, because a lambda's erased signature is the
     *  one its functional interface declares and `StreamEncoder`'s takes two. */
    protected static java.util.function.Function horizonEncoder(Object encoder) {
        if (encoder instanceof java.util.function.Function) return (java.util.function.Function) encoder;
        if (!(encoder instanceof net.minecraft.network.codec.StreamEncoder)) return null;
        net.minecraft.network.codec.StreamEncoder target = (net.minecraft.network.codec.StreamEncoder) encoder;
        return pair -> { Object[] argv = (Object[]) pair; target.encode((net.minecraft.network.FriendlyByteBuf) argv[0], argv[1]); return null; };
    }
    /** The decoder `of` was handed: a `StreamDecoder` is called with the buffer
     *  alone, which is the same shape as a `Function`'s one argument. */
    protected static java.util.function.Function horizonDecoder(Object decoder) {
        if (decoder instanceof java.util.function.Function) return (java.util.function.Function) decoder;
        if (!(decoder instanceof net.minecraft.network.codec.StreamDecoder)) return null;
        net.minecraft.network.codec.StreamDecoder target = (net.minecraft.network.codec.StreamDecoder) decoder;
        return buffer -> target.decode(buffer);
    }
    /** The member codecs in wire order, so `composite` can walk them without
     *  spelling out eight fields at every call site. */
    protected net.minecraft.network.codec.StreamCodec[] horizonMembers() {
        java.util.List<net.minecraft.network.codec.StreamCodec> members = new java.util.ArrayList<>();
        for (int i = 1; i <= 8; i++) { net.minecraft.network.codec.StreamCodec member = horizonMemberAt(i); if (member == null) break; members.add(member); }
        return members.toArray(new net.minecraft.network.codec.StreamCodec[0]);
    }
    /** The codec a dispatch table answers for one type head. */
    protected net.minecraft.network.codec.StreamCodec horizonDispatched(Object type) {
        return (net.minecraft.network.codec.StreamCodec) horizonSecond.apply(type);
    }
    /** A composite's value: the mod's constructor function over the decoded
     *  members, in the order it was handed them.
     *
     *  Vanilla's `composite` takes a `Function` for one member, a
     *  `BiFunction` for two and `Function3`… for more; the constructor is one
     *  of them, so the arity decides how it is called. */
    protected Object horizonConstruct(java.util.List values) {
        Object constructor = horizonConstructor;
        int count = values.size();
        if (constructor == null) throw new java.lang.IllegalStateException("horizon StreamCodec composite: no constructor function");
        if (constructor instanceof java.util.function.Function && count == 1) {
            return ((java.util.function.Function) constructor).apply(values.get(0));
        }
        if (constructor instanceof java.util.function.BiFunction && count == 2) {
            return ((java.util.function.BiFunction) constructor).apply(values.get(0), values.get(1));
        }
        // `Function3`..`Function12` name no common supertype: the lambda's own
        // `apply` is called reflectively.
        try {
            Class<?>[] types = new Class<?>[count];
            for (int i = 0; i < count; i++) types[i] = Object.class;
            java.lang.reflect.Method method = constructor.getClass().getMethod("apply", types);
            method.setAccessible(true);
            return method.invoke(constructor, values.toArray());
        } catch (java.lang.ReflectiveOperationException e) {
            throw new java.lang.IllegalStateException("horizon StreamCodec composite: the constructor function did not take " + count + " members", e);
        }
    }
    protected net.minecraft.network.codec.StreamCodec horizonMemberAt(int index) {
        return index == 1 ? horizonMember1 : index == 2 ? horizonMember2 : index == 3 ? horizonMember3 : index == 4 ? horizonMember4 : index == 5 ? horizonMember5 : index == 6 ? horizonMember6 : index == 7 ? horizonMember7 : index == 8 ? horizonMember8 : null;
    }
    /** A recursive codec's delegate, built once from the mod's factory. */
    protected net.minecraft.network.codec.StreamCodec horizonRecursive(net.minecraft.network.codec.StreamCodec self) {
        return (net.minecraft.network.codec.StreamCodec) ((java.util.function.UnaryOperator) horizonFirst).apply(self);
    }
    /** A composite's members decoded in order, which is the list the
     *  constructor function rebuilds the value from. */
    protected java.util.List horizonDecodeMembers(net.minecraft.network.FriendlyByteBuf buffer) {
        java.util.List<Object> values = new java.util.ArrayList<>();
        for (net.minecraft.network.codec.StreamCodec member : horizonMembers()) values.add(member.decode(buffer));
        return values;
    }
    protected java.util.function.Function horizonGetterAt(int index) {
        return index == 1 ? horizonGetters1 : index == 2 ? horizonGetters2 : index == 3 ? horizonGetters3 : index == 4 ? horizonGetters4 : index == 5 ? horizonGetters5 : index == 6 ? horizonGetters6 : index == 7 ? horizonGetters7 : index == 8 ? horizonGetters8 : null;
    }
"#;

/// `StreamEncoder`: the callback `StreamCodec.of` takes for its `encode` half.
const STREAM_ENCODER: &str = r#"package net.minecraft.network.codec;

public interface StreamEncoder<B, V> {
    void encode(B buffer, V value);
}
"#;

/// `StreamDecoder`: the callback `StreamCodec.of` takes for its `decode` half.
const STREAM_DECODER: &str = r#"package net.minecraft.network.codec;

public interface StreamDecoder<B, V> {
    V decode(B buffer);
}
"#;

/// `StreamMemberEncoder`: the buffer-then-member form `ofMember` takes.
const STREAM_MEMBER_ENCODER: &str = r#"package net.minecraft.network.codec;

public interface StreamMemberEncoder<B, V> {
    void encode(V member, B buffer);
}
"#;

/// The file the operation shim is written to, without `.java`.
const STREAM_CODEC_OPERATION_TYPE_FILE: &str = "net/minecraft/network/codec/StreamCodecOperation";

/// The operation type the generated `StreamCodec` interface's `apply` names.
const STREAM_CODEC_OPERATION_TYPE: &str = "net/minecraft/network/codec/StreamCodec$CodecOperation";

/// Vanilla's `StreamCodec.CodecOperation`, the functional interface `apply`
/// takes. It is a top-level file here, never nested: the façade writes the
/// game's interfaces with `default` bodies, so a nested abstract member would
/// shadow the name a mod writes (`codec -> ...` needs a target) and the codec's
/// own `apply` would resolve to itself.
const STREAM_CODEC_OPERATION: &str = r#"package net.minecraft.network.codec;

public interface StreamCodecOperation<B, S, T> {
    net.minecraft.network.codec.StreamCodec apply(net.minecraft.network.codec.StreamCodec codec);
}
"#;

/// Bounded stand-in for NeoForge's payload registrar. Every play/configuration
/// direction keeps chaining, and the payload type objects it was handed stay in
/// a static map keyed by that direction, so bootstrap can read the registered
/// `Identifier`s back (`horizonPayloadIds`) without a live channel.
///
/// `id()` is read reflectively so this shim has no game-class dependency and
/// stays compilable on a jar-only classpath.
const NEOFORGE_PAYLOAD_REGISTRAR_CLASS: &str =
    "net/neoforged/neoforge/network/registration/PayloadRegistrar";
const NEOFORGE_PAYLOAD_REGISTRAR: &str = r#"package net.neoforged.neoforge.network.registration;

import net.minecraft.network.RegistryFriendlyByteBuf;
import net.minecraft.network.codec.StreamCodec;
import net.minecraft.network.protocol.common.custom.CustomPacketPayload;
import net.neoforged.neoforge.network.handling.IPayloadContext;
import net.neoforged.neoforge.network.handling.IPayloadHandler;

public class PayloadRegistrar {
    private static final java.util.Map<String, java.util.List<CustomPacketPayload.Type>> horizonPayloads = new java.util.LinkedHashMap<>();
    private static final java.util.Map<String, java.util.List<StreamCodec>> horizonPayloadCodecs = new java.util.LinkedHashMap<>();
    private static final java.util.Map<String, java.util.List<String>> horizonPayloadVersions = new java.util.LinkedHashMap<>();
    private static final java.util.Map<String, java.util.List<Boolean>> horizonPayloadOptional = new java.util.LinkedHashMap<>();
    private static final java.util.Map<String, java.util.List<Object>> horizonLegacyPayloads = new java.util.LinkedHashMap<>();
    private int horizonRegistrationCount;
    private String horizonVersion;
    private boolean horizonIsOptional;

    public int horizonRegistrationCount() { return horizonRegistrationCount; }

    public static java.util.List<CustomPacketPayload.Type> horizonPayloadTypes(String direction) {
        java.util.List<CustomPacketPayload.Type> values = horizonPayloads.get(direction);
        return values == null ? new java.util.ArrayList<>() : new java.util.ArrayList<>(values);
    }

    public static java.util.List<Object> horizonPayloadIds(String direction) {
        java.util.List<Object> ids = new java.util.ArrayList<>();
        for (CustomPacketPayload.Type type : horizonPayloadTypes(direction)) {
            Object id = horizonPayloadId(type);
            if (id != null) ids.add(id);
        }
        java.util.List<Object> legacy = horizonLegacyPayloads.get(direction);
        if (legacy != null) for (Object type : legacy) { Object id = horizonPayloadId(type); if (id != null) ids.add(id); }
        return ids;
    }

    public static java.util.List<StreamCodec> horizonPayloadCodecs(String direction) {
        java.util.List<StreamCodec> values = horizonPayloadCodecs.get(direction);
        return values == null ? new java.util.ArrayList<>() : new java.util.ArrayList<>(values);
    }

    private static Object horizonPayloadId(Object type) {
        if (type == null) return null;
        try { java.lang.reflect.Method method = type.getClass().getMethod("id"); method.setAccessible(true); return method.invoke(type); } catch (Throwable ignored) { return null; }
    }

    public static java.util.List<String> horizonPayloadVersions(String direction) {
        java.util.List<String> values = horizonPayloadVersions.get(direction);
        return values == null ? new java.util.ArrayList<>() : new java.util.ArrayList<>(values);
    }

    public static java.util.List<Boolean> horizonPayloadOptional(String direction) {
        java.util.List<Boolean> values = horizonPayloadOptional.get(direction);
        return values == null ? new java.util.ArrayList<>() : new java.util.ArrayList<>(values);
    }

    private <T extends CustomPacketPayload> PayloadRegistrar horizonRetain(String direction, CustomPacketPayload.Type type, StreamCodec codec) {
        horizonPayloads.computeIfAbsent(direction, key -> new java.util.ArrayList<>()).add(type);
        horizonPayloadCodecs.computeIfAbsent(direction, key -> new java.util.ArrayList<>()).add(codec);
        horizonPayloadVersions.computeIfAbsent(direction, key -> new java.util.ArrayList<>()).add(horizonVersion);
        horizonPayloadOptional.computeIfAbsent(direction, key -> new java.util.ArrayList<>()).add(horizonIsOptional);
        horizonRegistrationCount++;
        horizonVersion = null;
        horizonIsOptional = false;
        return this;
    }

    private PayloadRegistrar horizonRetainLegacy(String direction, Object type, Object codec) {
        horizonLegacyPayloads.computeIfAbsent(direction, key -> new java.util.ArrayList<>()).add(type);
        horizonPayloadCodecs.computeIfAbsent(direction, key -> new java.util.ArrayList<>()).add(null);
        horizonPayloadVersions.computeIfAbsent(direction, key -> new java.util.ArrayList<>()).add(horizonVersion);
        horizonPayloadOptional.computeIfAbsent(direction, key -> new java.util.ArrayList<>()).add(horizonIsOptional);
        horizonRegistrationCount++;
        horizonVersion = null; horizonIsOptional = false;
        return this;
    }

    public <T extends CustomPacketPayload> PayloadRegistrar playToServer(CustomPacketPayload.Type type, StreamCodec codec, IPayloadHandler<T> handler) { return horizonRetain("playToServer", type, codec); }
    public <T extends CustomPacketPayload> PayloadRegistrar playToClient(CustomPacketPayload.Type type, StreamCodec codec, IPayloadHandler<T> handler) { return horizonRetain("playToClient", type, codec); }
    public <T extends CustomPacketPayload> PayloadRegistrar playBidirectional(CustomPacketPayload.Type type, StreamCodec codec, IPayloadHandler<T> handler) { return horizonRetain("playBidirectional", type, codec); }
    public <T extends CustomPacketPayload> PayloadRegistrar configurationToServer(CustomPacketPayload.Type type, StreamCodec codec, IPayloadHandler<T> handler) { return horizonRetain("configurationToServer", type, codec); }
    public <T extends CustomPacketPayload> PayloadRegistrar configurationToClient(CustomPacketPayload.Type type, StreamCodec codec, IPayloadHandler<T> handler) { return horizonRetain("configurationToClient", type, codec); }
    public <T extends CustomPacketPayload> PayloadRegistrar configurationBidirectional(CustomPacketPayload.Type type, StreamCodec codec, IPayloadHandler<T> handler) { return horizonRetain("configurationBidirectional", type, codec); }
    // Bounded legacy overloads: a payload marker that is not a
    // CustomPacketPayload.Type (a pre-1.20.5 codec shape, or the native
    // probe's hand-written Type) has no typed overload to bind to. The typed
    // overloads above carry the real API's descriptor; these keep the old call
    // site linkable and are listed in Report::legacy.
    public PayloadRegistrar playToServer(Object type, Object codec, Object handler) { return horizonRetainLegacy("playToServer", type, codec); }
    public PayloadRegistrar playToClient(Object type, Object codec, Object handler) { return horizonRetainLegacy("playToClient", type, codec); }
    public PayloadRegistrar playBidirectional(Object type, Object codec, Object handler) { return horizonRetainLegacy("playBidirectional", type, codec); }
    public PayloadRegistrar configurationToServer(Object type, Object codec, Object handler) { return horizonRetainLegacy("configurationToServer", type, codec); }
    public PayloadRegistrar configurationToClient(Object type, Object codec, Object handler) { return horizonRetainLegacy("configurationToClient", type, codec); }
    public PayloadRegistrar configurationBidirectional(Object type, Object codec, Object handler) { return horizonRetainLegacy("configurationBidirectional", type, codec); }
    public PayloadRegistrar optional() { horizonIsOptional = true; return this; }
    public PayloadRegistrar versioned(String version) { horizonVersion = version; return this; }
}
"#;

/// Bounded stand-in for Fabric API's `PayloadTypeRegistry`. Only emitted when
/// the real Fabric API class is absent from the target's classpath, so a
/// jar-only slice keeps `PayloadTypeRegistry.playS2C().register(...)` linking.
///
/// Registration stays a marker, but each direction keeps the payload type
/// objects it was handed in a static map, and `horizonPayloadIds` reads their
/// `id()` (reflection, so no game class is required here) when a caller wants
/// the `Identifier`s.
const FABRIC_PAYLOAD_TYPE_REGISTRY: &str = r#"package net.fabricmc.fabric.api.networking.v1;

public final class PayloadTypeRegistry {
    private static final java.util.Map<String, java.util.List<Object>> horizonPayloads = new java.util.LinkedHashMap<>();
    private static final java.util.Map<String, java.util.List<Object>> horizonPayloadCodecs = new java.util.LinkedHashMap<>();

    private PayloadTypeRegistry() {}

    public static <T> PayloadTypeRegistrar<T> playS2C() { return new PayloadTypeRegistrar<>("playToClient"); }
    public static <T> PayloadTypeRegistrar<T> playC2S() { return new PayloadTypeRegistrar<>("playToServer"); }
    public static <T> PayloadTypeRegistrar<T> configurationS2C() { return new PayloadTypeRegistrar<>("configurationToClient"); }
    public static <T> PayloadTypeRegistrar<T> configurationC2S() { return new PayloadTypeRegistrar<>("configurationToServer"); }

    public static java.util.List<Object> horizonPayloadTypes(String direction) {
        java.util.List<Object> values = horizonPayloads.get(direction);
        return values == null ? new java.util.ArrayList<>() : new java.util.ArrayList<>(values);
    }

    public static java.util.List<Object> horizonPayloadIds(String direction) {
        java.util.List<Object> ids = new java.util.ArrayList<>();
        for (Object type : horizonPayloadTypes(direction)) {
            Object id = horizonPayloadId(type);
            if (id != null) ids.add(id);
        }
        return ids;
    }

    public static java.util.List<Object> horizonPayloadCodecs(String direction) {
        java.util.List<Object> values = horizonPayloadCodecs.get(direction);
        return values == null ? new java.util.ArrayList<>() : new java.util.ArrayList<>(values);
    }

    private static Object horizonPayloadId(Object type) {
        if (type == null) return null;
        try { return type.getClass().getMethod("id").invoke(type); } catch (Throwable ignored) { return null; }
    }

    public static final class PayloadTypeRegistrar<T> {
        private final String horizonDirection;
        private int horizonRegistrationCount;

        PayloadTypeRegistrar(String direction) { this.horizonDirection = direction; }
        public int horizonRegistrationCount() { return horizonRegistrationCount; }
        public String horizonDirection() { return horizonDirection; }

        // Bounded on purpose: the real signature is
        // (CustomPacketPayload.Type<T>, StreamCodec<? super RegistryFriendlyByteBuf, T>),
        // but this class is the jar-only slice and must stay compilable without
        // a game jar, so both parameters stay Object. A source call links; a
        // precompiled call site is named in Report::legacy.
        public PayloadTypeRegistrar<T> register(Object type, Object codec) {
            horizonPayloads.computeIfAbsent(horizonDirection, key -> new java.util.ArrayList<>()).add(type);
            horizonPayloadCodecs.computeIfAbsent(horizonDirection, key -> new java.util.ArrayList<>()).add(codec);
            horizonRegistrationCount++;
            return this;
        }
    }
}
"#;

/// Bounded Fabric API `PacketSender`: the response sender a receiver context
/// hands back. Only the payload overload is modelled; a packet-object send is
/// a gap until the Host can carry it.
const FABRIC_PACKET_SENDER: &str = r#"package net.fabricmc.fabric.api.networking.v1;

public interface PacketSender {
    void sendPacket(net.minecraft.network.protocol.common.custom.CustomPacketPayload payload);
}
"#;

/// Bounded Fabric API `ServerPlayNetworking`. Registration is real: handlers
/// are retained per payload type id (the same key `Type#id()` yields), a second
/// registration for a type returns false and leaves the first handler in place,
/// and `canSend` answers from that map. `send` is a marker: it records the
/// direction and payload type id in a static list instead of moving a packet.
const FABRIC_SERVER_PLAY_NETWORKING: &str = r#"package net.fabricmc.fabric.api.networking.v1;

public final class ServerPlayNetworking {
    private static final java.util.Map<String, PlayPayloadHandler> horizonReceivers = new java.util.LinkedHashMap<>();
    private static final java.util.List<String> horizonSends = new java.util.ArrayList<>();

    private ServerPlayNetworking() {}

    public static boolean registerGlobalReceiver(net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type type, PlayPayloadHandler handler) {
        if (type == null || handler == null) return false;
        String id = horizonTypeId(type);
        if (horizonReceivers.containsKey(id)) return false;
        horizonReceivers.put(id, handler);
        return true;
    }

    public static boolean canSend(net.minecraft.server.level.ServerPlayer player, net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type type) {
        return type != null && horizonReceivers.containsKey(horizonTypeId(type));
    }

    public static void send(net.minecraft.server.level.ServerPlayer player, net.minecraft.network.protocol.common.custom.CustomPacketPayload payload) {
        horizonRecord("playToClient", payload);
    }

    /** The handler retained for a payload type id, or null. */
    public static PlayPayloadHandler horizonReceiver(String id) {
        return horizonReceivers.get(id);
    }

    /** Send markers recorded in call order as `direction typeId`. */
    public static java.util.List<String> horizonSends() {
        return new java.util.ArrayList<>(horizonSends);
    }

    private static String horizonTypeId(net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type type) {
        try {
            return type.id() == null ? "<null>" : type.id().toString();
        } catch (Throwable ignored) {
            return "<unknown>";
        }
    }

    private static void horizonRecord(String direction, net.minecraft.network.protocol.common.custom.CustomPacketPayload payload) {
        String id = "<unknown>";
        if (payload != null) {
            try {
                net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type type = payload.type();
                if (type != null && type.id() != null) id = type.id().toString();
            } catch (Throwable ignored) {
            }
        }
        horizonSends.add(direction + " " + id);
    }

    @FunctionalInterface
    public interface PlayPayloadHandler {
        void receive(net.minecraft.network.protocol.common.custom.CustomPacketPayload payload, Context context);
    }

    /** The packet-API name for the same bounded callback shape. */
    @FunctionalInterface
    public interface PlayChannelHandler {
        void receive(net.minecraft.network.protocol.common.custom.CustomPacketPayload payload, Context context);
    }

    public interface Context {
        net.minecraft.server.level.ServerPlayer player();
        net.fabricmc.fabric.api.networking.v1.PacketSender responseSender();
    }
}
"#;

/// Bounded Fabric API `ClientPlayNetworking`, the client package the real API
/// uses. It keeps the same per-type receiver map and `canSend` answer as the
/// server holder; `send` and `sendToServer` record `playToServer` markers.
/// `{player}` is filled with the resolved 26.3 client player class.
const FABRIC_CLIENT_PLAY_NETWORKING: &str = r#"package net.fabricmc.fabric.api.client.networking.v1;

public final class ClientPlayNetworking {
    private static final java.util.Map<String, PlayPayloadHandler> horizonReceivers = new java.util.LinkedHashMap<>();
    private static final java.util.List<String> horizonSends = new java.util.ArrayList<>();

    private ClientPlayNetworking() {}

    public static boolean registerGlobalReceiver(net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type type, PlayPayloadHandler handler) {
        if (type == null || handler == null) return false;
        String id = horizonTypeId(type);
        if (horizonReceivers.containsKey(id)) return false;
        horizonReceivers.put(id, handler);
        return true;
    }

    public static boolean canSend(net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type type) {
        return type != null && horizonReceivers.containsKey(horizonTypeId(type));
    }

    public static boolean canSend(net.minecraft.client.multiplayer.ClientPacketListener listener, net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type type) {
        return canSend(type);
    }

    public static void send(net.minecraft.network.protocol.common.custom.CustomPacketPayload payload) {
        horizonRecord("playToServer", payload);
    }

    public static void sendToServer(net.minecraft.network.protocol.common.custom.CustomPacketPayload payload) {
        horizonRecord("playToServer", payload);
    }

    /** The handler retained for a payload type id, or null. */
    public static PlayPayloadHandler horizonReceiver(String id) {
        return horizonReceivers.get(id);
    }

    /** Send markers recorded in call order as `direction typeId`. */
    public static java.util.List<String> horizonSends() {
        return new java.util.ArrayList<>(horizonSends);
    }

    private static String horizonTypeId(net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type type) {
        try {
            return type.id() == null ? "<null>" : type.id().toString();
        } catch (Throwable ignored) {
            return "<unknown>";
        }
    }

    private static void horizonRecord(String direction, net.minecraft.network.protocol.common.custom.CustomPacketPayload payload) {
        String id = "<unknown>";
        if (payload != null) {
            try {
                net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type type = payload.type();
                if (type != null && type.id() != null) id = type.id().toString();
            } catch (Throwable ignored) {
            }
        }
        horizonSends.add(direction + " " + id);
    }

    @FunctionalInterface
    public interface PlayPayloadHandler {
        void receive(net.minecraft.network.protocol.common.custom.CustomPacketPayload payload, Context context);
    }

    /** The packet-API name for the same bounded callback shape. */
    @FunctionalInterface
    public interface PlayChannelHandler {
        void receive(net.minecraft.network.protocol.common.custom.CustomPacketPayload payload, Context context);
    }

    public interface Context {
        {player} player();
        net.fabricmc.fabric.api.networking.v1.PacketSender responseSender();
    }
}
"#;

/// Bounded Fabric API `Event`: listeners stay in registration order and
/// `invoker()` returns their combined handler. The interface mirrors the two
/// members a mod calls (`register`, `invoker`); phases are not modelled, so a
/// holder that needs them stays a bounded gap. Nothing here runs gameplay.
const FABRIC_EVENT: &str = r#"package net.fabricmc.fabric.api.event;

public interface Event<T> {
    void register(T listener);
    T invoker();

    static <T> Event<T> horizon(java.util.function.Function<java.util.List<T>, T> dispatch) {
        return new HorizonEvent<>(dispatch);
    }
}

final class HorizonEvent<T> implements Event<T> {
    private final java.util.List<T> horizonListeners = new java.util.ArrayList<>();
    private final java.util.function.Function<java.util.List<T>, T> horizonDispatch;

    HorizonEvent(java.util.function.Function<java.util.List<T>, T> dispatch) { this.horizonDispatch = dispatch; }
    @Override public void register(T listener) { if (listener != null) horizonListeners.add(listener); }
    @Override public T invoker() {
        // Snapshot: a handler that registers another handler cannot change a
        // dispatch already in flight, and the order is registration order.
        return horizonDispatch.apply(java.util.Collections.unmodifiableList(new java.util.ArrayList<>(horizonListeners)));
    }
    @Override public String toString() { return "Event[" + horizonListeners.size() + " listeners]"; }
}
"#;

/// One bounded Fabric API event as
/// `(field, callback, method, parameters, call arguments)`: the holder's field,
/// the nested callback interface and the invoker body that walks the listeners
/// in registration order. Parameter types are fully qualified so the emitted
/// holder needs no imports the caller's classpath may not carry.
type FabricEvent = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
);

const SERVER_PARAMS: &str = "net.minecraft.server.MinecraftServer p0";
const CLIENT_PARAMS: &str = "net.minecraft.client.Minecraft p0";
const PLAYER_PARAMS: &str = "net.minecraft.server.level.ServerPlayer p0, net.minecraft.server.level.ServerPlayer p1, boolean p2";

const FABRIC_SERVER_LIFECYCLE: &[FabricEvent] = &[
    (
        "SERVER_STARTING",
        "ServerStarting",
        "onServerStarting",
        SERVER_PARAMS,
        "p0",
    ),
    (
        "SERVER_STARTED",
        "ServerStarted",
        "onServerStarted",
        SERVER_PARAMS,
        "p0",
    ),
    (
        "SERVER_STOPPING",
        "ServerStopping",
        "onServerStopping",
        SERVER_PARAMS,
        "p0",
    ),
];

const FABRIC_CLIENT_LIFECYCLE: &[FabricEvent] = &[
    (
        "CLIENT_STARTED",
        "ClientStarted",
        "onClientStarted",
        CLIENT_PARAMS,
        "p0",
    ),
    (
        "CLIENT_STOPPING",
        "ClientStopping",
        "onClientStopping",
        CLIENT_PARAMS,
        "p0",
    ),
];

/// `ClientTickEvents`: the two per-tick callbacks a client initializer attaches
/// to. Same `Event` shape as the lifecycle holders, so registration order and
/// `invoker()` behave identically.
const FABRIC_CLIENT_TICK_EVENTS: &[FabricEvent] = &[
    (
        "START_CLIENT_TICK",
        "StartTick",
        "onStartTick",
        CLIENT_PARAMS,
        "p0",
    ),
    (
        "END_CLIENT_TICK",
        "EndTick",
        "onEndTick",
        CLIENT_PARAMS,
        "p0",
    ),
];

const FABRIC_SERVER_PLAYER_EVENTS: &[FabricEvent] = &[
    (
        "AFTER_RESPAWN",
        "AfterRespawn",
        "afterRespawn",
        PLAYER_PARAMS,
        "p0, p1, p2",
    ),
    (
        "COPY_FROM",
        "CopyFrom",
        "copyFrom",
        PLAYER_PARAMS,
        "p0, p1, p2",
    ),
];

fn fabric_event_source(package: &str, class: &str, doc: &str, events: &[FabricEvent]) -> String {
    let mut out = String::new();
    // Callers pass the API class's path (`a/b/C`) or its Java package
    // (`a.b`); a package declaration only accepts the dotted form.
    let _ = writeln!(out, "package {};", package.replace('/', "."));
    let _ = writeln!(out);
    let _ = writeln!(out, "import net.fabricmc.fabric.api.event.Event;");
    let _ = writeln!(out);
    let _ = writeln!(out, "/** {doc} */");
    let _ = writeln!(out, "public final class {class} {{");
    let _ = writeln!(out, "    private {class}() {{}}");
    for (field, callback, method, params, args) in events {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "    public static final Event<{callback}> {field} = Event.horizon(horizonListeners ->"
        );
        let _ = writeln!(out, "        ({params}) -> {{");
        let _ = writeln!(
            out,
            "            for ({callback} horizonListener : horizonListeners) horizonListener.{method}({args});"
        );
        let _ = writeln!(out, "        }});");
    }
    for (_, callback, method, params, _) in events {
        let _ = writeln!(out);
        let _ = writeln!(out, "    @FunctionalInterface");
        let _ = writeln!(out, "    public interface {callback} {{");
        let _ = writeln!(out, "        void {method}({params});");
        let _ = writeln!(out, "    }}");
    }
    let _ = writeln!(out, "}}");
    out
}

/// Fabric API's command registration event. `EVENT` is a functional
/// `CommandRegistrationCallback`, exactly as in the real API, so
/// `EVENT.register((dispatcher, registryAccess, environment) -> ...)` compiles.
const FABRIC_COMMAND_REGISTRATION_CALLBACK: &str = r#"package net.fabricmc.fabric.api.command.v2;

import com.mojang.brigadier.CommandDispatcher;
import net.fabricmc.fabric.api.event.Event;
import net.minecraft.commands.CommandBuildContext;
import net.minecraft.commands.CommandSourceStack;
import net.minecraft.commands.Commands;

@FunctionalInterface
public interface CommandRegistrationCallback {
    void register(CommandDispatcher<CommandSourceStack> dispatcher, CommandBuildContext registryAccess, Commands.CommandSelection environment);

    Event<CommandRegistrationCallback> EVENT = Event.horizon(horizonListeners ->
        (dispatcher, registryAccess, environment) -> {
            for (CommandRegistrationCallback horizonListener : horizonListeners) {
                horizonListener.register(dispatcher, registryAccess, environment);
            }
        });
}
"#;

/// Fabric API's client HUD callback. `DrawContext`/`RenderTickCounter` are the
/// 26.3 slice's resolved names for the GUI and tick-counter values; the holder
/// is a functional interface with a static `EVENT`, exactly as the real API, so
/// `EVENT.register((context, ticks) -> ...)` compiles and dispatch order is the
/// registration order. Nothing is drawn: the host calls the invoker per frame.
const FABRIC_HUD_RENDER_CALLBACK: &str = r#"package net.fabricmc.fabric.api.client.rendering.v1;

import net.fabricmc.fabric.api.event.Event;
import {gui};
import {ticks};

@FunctionalInterface
public interface HudRenderCallback {
    void onHudRender({gui} drawContext, {ticks} tickCounter);

    Event<HudRenderCallback> EVENT = Event.horizon(horizonListeners ->
        (drawContext, tickCounter) -> {
            for (HudRenderCallback horizonListener : horizonListeners) {
                horizonListener.onHudRender(drawContext, tickCounter);
            }
        });
}
"#;

/// Fabric API's client screen events: one bounded `Event` per screen hook.
/// Listeners are retained in registration order and `invoker()` walks that
/// snapshot, so a hook registered during a dispatch cannot change the one in
/// flight. `AFTER_INIT` is the four-argument shape a ported screen registers
/// (`(client, screen, scaledWidth, scaledHeight) -> ...`); the two `ALLOW_*`
/// hooks answer `true` unless a listener vetoes, which is how the host cancels
/// the click or key press. Nothing opens, draws or clicks a screen here: the
/// host posts each hook at the point its name says.
const FABRIC_SCREEN_EVENTS: &str = "net/fabricmc/fabric/api/client/screen/v1/ScreenEvents";
const FABRIC_SCREEN_EVENTS_SOURCE: &str = r#"package net.fabricmc.fabric.api.client.screen.v1;

import net.fabricmc.fabric.api.event.Event;
import {gui};
import {minecraft};
import {screen};

public final class ScreenEvents {
    private ScreenEvents() {}

    public static final Event<BeforeInit> BEFORE_INIT = Event.horizon(horizonListeners ->
        (client, screen, scaledWidth, scaledHeight) -> {
            for (BeforeInit horizonListener : horizonListeners) horizonListener.beforeInit(client, screen, scaledWidth, scaledHeight);
        });
    public static final Event<AfterInit> AFTER_INIT = Event.horizon(horizonListeners ->
        (client, screen, scaledWidth, scaledHeight) -> {
            for (AfterInit horizonListener : horizonListeners) horizonListener.afterInit(client, screen, scaledWidth, scaledHeight);
        });
    public static final Event<BeforeRender> BEFORE_RENDER = Event.horizon(horizonListeners ->
        (screen, context, mouseX, mouseY, tickDelta) -> {
            for (BeforeRender horizonListener : horizonListeners) horizonListener.beforeRender(screen, context, mouseX, mouseY, tickDelta);
        });
    public static final Event<AfterRender> AFTER_RENDER = Event.horizon(horizonListeners ->
        (screen, context, mouseX, mouseY, tickDelta) -> {
            for (AfterRender horizonListener : horizonListeners) horizonListener.afterRender(screen, context, mouseX, mouseY, tickDelta);
        });
    public static final Event<AllowMouseClick> ALLOW_MOUSE_CLICK = Event.horizon(horizonListeners ->
        (screen, mouseX, mouseY, button) -> {
            for (AllowMouseClick horizonListener : horizonListeners) if (!horizonListener.allowMouseClick(screen, mouseX, mouseY, button)) return false;
            return true;
        });
    public static final Event<AllowKeyPress> ALLOW_KEY_PRESS = Event.horizon(horizonListeners ->
        (screen, keyCode, scanCode, modifiers) -> {
            for (AllowKeyPress horizonListener : horizonListeners) if (!horizonListener.allowKeyPress(screen, keyCode, scanCode, modifiers)) return false;
            return true;
        });

    @FunctionalInterface
    public interface BeforeInit { void beforeInit({minecraft} client, {screen} screen, int scaledWidth, int scaledHeight); }
    @FunctionalInterface
    public interface AfterInit { void afterInit({minecraft} client, {screen} screen, int scaledWidth, int scaledHeight); }
    @FunctionalInterface
    public interface BeforeRender { void beforeRender({screen} screen, {gui} context, int mouseX, int mouseY, float tickDelta); }
    @FunctionalInterface
    public interface AfterRender { void afterRender({screen} screen, {gui} context, int mouseX, int mouseY, float tickDelta); }
    @FunctionalInterface
    public interface AllowMouseClick { boolean allowMouseClick({screen} screen, double mouseX, double mouseY, int button); }
    @FunctionalInterface
    public interface AllowKeyPress { boolean allowKeyPress({screen} screen, int keyCode, int scanCode, int modifiers); }
}
"#;

/// Fabric API's `Screens` helper: the widget lookups a screen makes while it
/// initializes. The host owns the widget tree, so these are bounded markers —
/// no buttons exist behind the façade and `getWidget`/`getFocused` answer
/// `null`, which stops a port at the lookup that has to be wired.
const FABRIC_SCREENS: &str = "net/fabricmc/fabric/api/client/screen/v1/Screens";
const FABRIC_SCREENS_SOURCE: &str = r#"package net.fabricmc.fabric.api.client.screen.v1;

import java.util.Collections;
import java.util.List;
import {clickable_widget};
import {screen};

public final class Screens {
    private Screens() {}

    public static List<{clickable_widget}> getButtons({screen} screen) {
        return Collections.emptyList();
    }

    public static <T extends {clickable_widget}> T getWidget({screen} screen, Class<T> widgetClass) {
        return null;
    }

    public static <T extends {clickable_widget}> T getFocused({screen} screen, Class<T> widgetClass) {
        return null;
    }
}
"#;

/// Fabric API's item tooltip callback: listeners may append to the mutable
/// line list the host passes, in registration order. The stack, tooltip
/// context, flag and text types are the resolved game types, because a mod
/// that implements the interface explicitly writes them in its override.
const FABRIC_ITEM_TOOLTIP_CALLBACK: &str = r#"package net.fabricmc.fabric.api.client.item.tooltip.v1;

import java.util.List;
import net.fabricmc.fabric.api.event.Event;
import {item_stack};
import {tooltip_context};
import {tooltip_type};
import {component};

@FunctionalInterface
public interface ItemTooltipCallback {
    void getTooltip({item_stack} stack, {tooltip_context} tooltipContext, {tooltip_type} tooltipType, List<{component}> lines);

    Event<ItemTooltipCallback> EVENT = Event.horizon(horizonListeners ->
        (stack, tooltipContext, tooltipType, lines) -> {
            for (ItemTooltipCallback horizonListener : horizonListeners) {
                horizonListener.getTooltip(stack, tooltipContext, tooltipType, lines);
            }
        });
}
"#;

/// Fabric API's key-binding registry. `registerKeyBinding` returns the mapping
/// it was handed (a mod keeps the reference to read its `consumeClick` state),
/// and the registrations are retained in order so bootstrap can be inspected
/// without a live options screen.
const FABRIC_KEY_BINDING_HELPER: &str = r#"package net.fabricmc.fabric.api.client.keybinding.v1;

import {key_mapping};

public final class KeyBindingHelper {
    private static final java.util.List<{key_mapping}> HORIZON_BINDINGS = new java.util.ArrayList<>();

    private KeyBindingHelper() {}

    public static {key_mapping} registerKeyBinding({key_mapping} binding) {
        if (binding != null) HORIZON_BINDINGS.add(binding);
        return binding;
    }

    public static java.util.List<{key_mapping}> horizonKeyBindings() {
        return java.util.Collections.unmodifiableList(HORIZON_BINDINGS);
    }
}
"#;

/// Fabric API's item-group registration: one bounded `Event` per creative tab,
/// in first-use order, whose listeners append to the tab's entry list in
/// registration order. 26.3's own API renamed this holder
/// (`CreativeModeTabEvents#modifyOutputEvent`/`ModifyOutput`); the older
/// `ItemGroupEvents` name is kept because that is the class a ported jar names.
const FABRIC_ITEM_GROUP_EVENTS: &str = r#"package net.fabricmc.fabric.api.itemgroup.v1;

import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import net.fabricmc.fabric.api.event.Event;
import {creative_mode_tab};
import {feature_flag_set};
import {resource_key};

public final class ItemGroupEvents {
    private static final Map<{resource_key}, Event<ModifyEntries>> HORIZON_EVENTS = new LinkedHashMap<>();

    private ItemGroupEvents() {}

    public static Event<ModifyEntries> modifyEntriesEvent({resource_key} tab) {
        Event<ModifyEntries> existing = HORIZON_EVENTS.get(tab);
        if (existing != null) return existing;
        java.util.function.Function<List<ModifyEntries>, ModifyEntries> dispatch =
            horizonListeners -> (entries, enabledFeatures) -> {
                for (ModifyEntries horizonListener : horizonListeners) {
                    horizonListener.modifyEntries(entries, enabledFeatures);
                }
            };
        Event<ModifyEntries> event = Event.horizon(dispatch);
        HORIZON_EVENTS.put(tab, event);
        return event;
    }

    /** The tabs whose event was created, in first-use order. */
    public static List<{resource_key}> horizonTabs() {
        return Collections.unmodifiableList(new ArrayList<>(HORIZON_EVENTS.keySet()));
    }

    @FunctionalInterface
    public interface ModifyEntries {
        void modifyEntries(FabricItemGroupEntries entries, {feature_flag_set} enabledFeatures);
    }
}
"#;

/// The tab's entry list as the mod-visible value: `add`/`accept` append,
/// `addAfter`/`addBefore` insert relative to a stack already in the list (and
/// append when the anchor is absent, so the outcome stays deterministic), and
/// the accessors hand back unmodifiable views in that order.
const FABRIC_ITEM_GROUP_ENTRIES: &str = r#"package net.fabricmc.fabric.api.itemgroup.v1;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import {item_stack};

public final class FabricItemGroupEntries {
    private final List<{item_stack}> horizonStacks = new ArrayList<>();

    public FabricItemGroupEntries() {}

    public void add({item_stack} stack) {
        if (stack != null) horizonStacks.add(stack);
    }

    public void accept({item_stack} stack) {
        add(stack);
    }

    public void addAfter({item_stack} anchor, {item_stack}... stacks) {
        int at = horizonStacks.indexOf(anchor);
        at = at < 0 ? horizonStacks.size() : at + 1;
        for ({item_stack} stack : stacks) {
            if (stack != null) horizonStacks.add(at++, stack);
        }
    }

    public void addBefore({item_stack} anchor, {item_stack}... stacks) {
        int at = horizonStacks.indexOf(anchor);
        at = at < 0 ? horizonStacks.size() : at;
        for ({item_stack} stack : stacks) {
            if (stack != null) horizonStacks.add(at++, stack);
        }
    }

    public List<{item_stack}> getDisplayStacks() {
        return Collections.unmodifiableList(horizonStacks);
    }

    public List<{item_stack}> getSearchTabStacks() {
        return getDisplayStacks();
    }
}
"#;

/// Fabric's entity renderer registry: `{type, provider}` pairs are retained in
/// registration order for the host, which owns every renderer. Nothing draws.
const FABRIC_ENTITY_RENDERER_REGISTRY: &str = r#"package net.fabricmc.fabric.api.client.rendering.v1;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import {entity_renderer_provider};
import {entity_type};

public final class EntityRendererRegistry {
    private static final List<Object[]> HORIZON_RENDERERS = new ArrayList<>();

    private EntityRendererRegistry() {}
    public static void register({entity_type} type, {entity_renderer_provider} provider) {
        if (type != null && provider != null) HORIZON_RENDERERS.add(new Object[] { type, provider });
    }
    /** `{type, provider}` pairs in registration order. */
    public static List<Object[]> horizonRenderers() {
        return Collections.unmodifiableList(new ArrayList<>(HORIZON_RENDERERS));
    }
}
"#;

/// Fabric's block-entity renderer registry: the same retention list as the
/// entity one, keyed by the block-entity type.
const FABRIC_BLOCK_ENTITY_RENDERER_REGISTRY: &str = r#"package net.fabricmc.fabric.api.client.rendering.v1;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import {block_entity_renderer_provider};
import {block_entity_type};

public final class BlockEntityRendererRegistry {
    private static final List<Object[]> HORIZON_RENDERERS = new ArrayList<>();

    private BlockEntityRendererRegistry() {}
    public static void register({block_entity_type} type, {block_entity_renderer_provider} provider) {
        if (type != null && provider != null) HORIZON_RENDERERS.add(new Object[] { type, provider });
    }
    /** `{type, provider}` pairs in registration order. */
    public static List<Object[]> horizonRenderers() {
        return Collections.unmodifiableList(new ArrayList<>(HORIZON_RENDERERS));
    }
}
"#;

/// Fabric's model-layer registry: the location/provider pairs are retained; the
/// provider stays a lambda target so a constructor reference from mod bytecode
/// still links.
const FABRIC_MODEL_LAYER_REGISTRY: &str = r#"package net.fabricmc.fabric.api.client.rendering.v1;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import {model_layer_location};
import {textured_model_data};

public final class ModelLayerRegistry {
    private static final List<Object[]> HORIZON_LAYERS = new ArrayList<>();

    private ModelLayerRegistry() {}
    @FunctionalInterface
    public interface TexturedModelDataProvider {
        {textured_model_data} createTexturedModelData();
    }
    public static void registerModelLayer({model_layer_location} location, TexturedModelDataProvider provider) {
        if (location != null && provider != null) HORIZON_LAYERS.add(new Object[] { location, provider });
    }
    /** `{location, provider}` pairs in registration order. */
    public static List<Object[]> horizonModelLayers() {
        return Collections.unmodifiableList(new ArrayList<>(HORIZON_LAYERS));
    }
}
"#;

/// Fabric's colour-provider registry as a marker: `register` keeps the provider
/// and hands it back, because computing a tint is the host's work. `BLOCK` and
/// `ITEM` are the two registries a mod names.
const FABRIC_COLOR_PROVIDER_REGISTRY: &str = r#"package net.fabricmc.fabric.api.client.rendering.v1;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

public final class ColorProviderRegistry {
    public interface ColorProvider {}
    @FunctionalInterface public interface BlockColorProvider extends ColorProvider {
        int getColor({block_state} state, {block_and_tint_getter} world, {block_pos} pos, int tintIndex);
    }
    @FunctionalInterface public interface ItemColorProvider extends ColorProvider {
        int getColor({item_stack} stack, int tintIndex);
    }

    public static final ColorProviderRegistry BLOCK = new ColorProviderRegistry();
    public static final ColorProviderRegistry ITEM = new ColorProviderRegistry();

    private static final List<ColorProvider> HORIZON_PROVIDERS = new ArrayList<>();

    private ColorProviderRegistry() {}
    // The provider interfaces keep the real 26.3 descriptors (`BlockState`,
    // `BlockAndTintGetter`, `BlockPos`); the single-target overloads are typed
    // per role rather than `Object`, so a call site's invoke names a resolvable
    // game class on both registries.
    public void register(BlockColorProvider provider, {block}... blocks) {
        if (provider != null) HORIZON_PROVIDERS.add(provider);
    }
    public void register(ItemColorProvider provider, {item}... items) {
        if (provider != null) HORIZON_PROVIDERS.add(provider);
    }
    public ColorProvider register({block} target, ColorProvider provider) {
        if (provider != null) HORIZON_PROVIDERS.add(provider);
        return provider;
    }
    public ColorProvider register({item} target, ColorProvider provider) {
        if (provider != null) HORIZON_PROVIDERS.add(provider);
        return provider;
    }
    public static List<ColorProvider> horizonColorProviders() {
        return Collections.unmodifiableList(new ArrayList<>(HORIZON_PROVIDERS));
    }
}
"#;

/// Fabric's client model-loading API, as library classes. The registry, the
/// three provider interfaces and the context a provider is handed are emitted
/// only when the real API class is absent; the host owns baking, so every
/// registration is retained as a value.
const FABRIC_MODEL_LOADING_REGISTRY: &str =
    "net/fabricmc/fabric/api/client/model/loading/v1/ModelLoadingRegistry";
const FABRIC_MODEL_PROVIDER_CONTEXT: &str =
    "net/fabricmc/fabric/api/client/model/loading/v1/ModelProviderContext";
const FABRIC_MODEL_RESOURCE_PROVIDER: &str =
    "net/fabricmc/fabric/api/client/model/loading/v1/ModelResourceProvider";
const FABRIC_MODEL_VARIANT_PROVIDER: &str =
    "net/fabricmc/fabric/api/client/model/loading/v1/ModelVariantProvider";
const FABRIC_MODEL_APPENDER: &str = "net/fabricmc/fabric/api/client/model/loading/v1/ModelAppender";

/// The context a model provider is handed: `loadModel` is a bounded marker
/// that records the id it was asked for (in call order) and answers no model,
/// because the façade has no baker. The overload with a loader delegates, so a
/// provider's second call site links too. Ids live in one ordered list because
/// an interface has no per-instance state; the host reads it back as a copy.
const FABRIC_MODEL_PROVIDER_CONTEXT_SOURCE: &str = r#"package net.fabricmc.fabric.api.client.model.loading.v1;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import {baked_model};
import {identifier};
import {model_loader};

public interface ModelProviderContext {
    List<{identifier}> HORIZON_REQUESTED_MODELS = Collections.synchronizedList(new ArrayList<>());
    default {baked_model} loadModel({identifier} id) {
        if (id != null) HORIZON_REQUESTED_MODELS.add(id);
        return null;
    }
    default {baked_model} loadModel({identifier} id, {model_loader} loader) {
        return loadModel(id);
    }
    /** The ids a provider asked for, in call order. */
    static List<{identifier}> horizonRequestedModels() {
        return Collections.unmodifiableList(new ArrayList<>(HORIZON_REQUESTED_MODELS));
    }
}
"#;

/// A model resource provider: the id it was registered for and the context it
/// may query. It answers no model here; retaining the provider is the slice.
const FABRIC_MODEL_RESOURCE_PROVIDER_SOURCE: &str = r#"package net.fabricmc.fabric.api.client.model.loading.v1;

import {identifier};
import {unbaked_model};

@FunctionalInterface
public interface ModelResourceProvider {
    {unbaked_model} loadModelResource({identifier} id, ModelProviderContext context);
}
"#;

/// A model variant provider: the variant id it is asked about and the context.
/// 26.3 keys a variant by its identifier, so this is the same shape as the
/// resource provider without the registration id.
const FABRIC_MODEL_VARIANT_PROVIDER_SOURCE: &str = r#"package net.fabricmc.fabric.api.client.model.loading.v1;

import {identifier};
import {unbaked_model};

@FunctionalInterface
public interface ModelVariantProvider {
    {unbaked_model} loadModelVariant({identifier} id, ModelProviderContext context);
}
"#;

/// A model appender: it adds ids to the consumer the host passes. The real API
/// uses its own `ModelConsumer`; the bounded shape takes the JDK consumer for
/// the ids, so no fourth API class has to be invented.
const FABRIC_MODEL_APPENDER_SOURCE: &str = r#"package net.fabricmc.fabric.api.client.model.loading.v1;

import java.util.function.Consumer;
import {identifier};

@FunctionalInterface
public interface ModelAppender {
    void appendAll(ModelProviderContext context, Consumer<{identifier}> consumer);
}
"#;

/// The registry retaining what a mod registers: one ordered ledger of
/// `{kind, id, provider}` triples, so a host reads registrations back in call
/// order and by kind. Nothing is baked, discovered or appended by itself.
const FABRIC_MODEL_LOADING_REGISTRY_SOURCE: &str = r#"package net.fabricmc.fabric.api.client.model.loading.v1;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import {identifier};

public final class ModelLoadingRegistry {
    private static final List<Object[]> HORIZON_REGISTRATIONS = new ArrayList<>();
    private ModelLoadingRegistry() {}
    public static void registerModelProvider({identifier} id, ModelResourceProvider provider) {
        horizonAdd("model", id, provider);
    }
    public static void registerModelVariantProvider(ModelVariantProvider provider) {
        horizonAdd("variant", null, provider);
    }
    public static void registerResourceProvider(ModelResourceProvider provider) {
        horizonAdd("resource", null, provider);
    }
    public static void registerAppender(ModelAppender appender) {
        horizonAdd("appender", null, appender);
    }
    private static void horizonAdd(String kind, {identifier} id, Object provider) {
        if (provider != null) HORIZON_REGISTRATIONS.add(new Object[] { kind, id, provider });
    }
    /** `{kind, id, provider}` triples in registration order. */
    public static List<Object[]> horizonRegistrations() {
        return Collections.unmodifiableList(new ArrayList<>(HORIZON_REGISTRATIONS));
    }
}
"#;

/// Fabric API's data-attachment types and registry. A registration is only
/// retained (id, codec and initializer markers, persistent and copy-on-death
/// flags); the store itself lives on the `AttachmentTarget` interface that the
/// game classes below implement in the façade.
const FABRIC_ATTACHMENT_TARGET: &str = "net/fabricmc/fabric/api/attachment/v1/AttachmentTarget";

/// The game classes Fabric's Mixin gives an attachment store to. A mod that
/// casts one of these to `AttachmentTarget` links through the interface's
/// default methods; a class this jar does not carry is simply not named.
const FABRIC_ATTACHMENT_TARGETS: &[&str] = &[
    "net/minecraft/world/entity/Entity",
    "net/minecraft/world/level/block/entity/BlockEntity",
    "net/minecraft/world/level/Level",
    "net/minecraft/world/item/ItemStack",
];

/// The registered attachment: `id()` answers the identifier the registry
/// retained, which is the same key the host reads back.
const FABRIC_ATTACHMENT_TYPE: &str = r#"package net.fabricmc.fabric.api.attachment.v1;

public interface AttachmentType<A> {
    {identifier} id();
}
"#;

/// Bounded Fabric API `AttachmentTarget`: the per-target store Fabric gives a
/// game class through its Mixin. The façade has no weaver, so one bounded
/// identity map holds the values for every target, and the game classes a mod
/// names implement this interface (`FABRIC_ATTACHMENT_TARGETS`). Values are
/// retained, never serialized, and no game thread is touched.
const FABRIC_ATTACHMENT_TARGET_SOURCE: &str = r#"package net.fabricmc.fabric.api.attachment.v1;

public interface AttachmentTarget {
    java.util.Map<Object, java.util.Map<String, Object>> HORIZON_ATTACHMENTS = java.util.Collections.synchronizedMap(new java.util.WeakHashMap<Object, java.util.Map<String, Object>>());

    static java.util.Map<String, Object> horizonAttachments(Object target) {
        if (target == null) return new java.util.LinkedHashMap<>();
        return HORIZON_ATTACHMENTS.computeIfAbsent(target, key -> new java.util.LinkedHashMap<>());
    }

    static String horizonKey(AttachmentType<?> type) {
        if (type == null || type.id() == null) return "<unknown>";
        return String.valueOf(type.id());
    }

    @SuppressWarnings("unchecked")
    static <A> A horizonCast(Object value) { return (A) value; }

    default <A> A getAttached(AttachmentType<A> type) { return horizonCast(horizonAttachments(this).get(horizonKey(type))); }

    default <A> boolean hasAttached(AttachmentType<A> type) { return horizonAttachments(this).containsKey(horizonKey(type)); }

    default <A> A getAttachedOrElse(AttachmentType<A> type, A defaultValue) {
        Object value = horizonAttachments(this).get(horizonKey(type));
        return value == null ? defaultValue : horizonCast(value);
    }

    default <A> A getAttachedOrGet(AttachmentType<A> type, java.util.function.Supplier<A> factory) {
        Object value = horizonAttachments(this).get(horizonKey(type));
        if (value != null) return horizonCast(value);
        A created = factory == null ? null : factory.get();
        if (created == null) return null;
        setAttached(type, created);
        return created;
    }

    default <A> A getAttachedOrCreate(AttachmentType<A> type, java.util.function.Supplier<A> factory) { return getAttachedOrGet(type, factory); }

    /** Bounded: the initializer is only a marker here, so an absent value stays null. */
    default <A> A getAttachedOrCreate(AttachmentType<A> type) { return getAttachedOrGet(type, () -> null); }

    default <A> A getAttachedOrSet(AttachmentType<A> type, A defaultValue) {
        Object value = horizonAttachments(this).get(horizonKey(type));
        if (value != null) return horizonCast(value);
        setAttached(type, defaultValue);
        return defaultValue;
    }

    /** Bounded: an absent value is left absent rather than handed to the modifier as null. */
    default <A> void modifyAttached(AttachmentType<A> type, java.util.function.UnaryOperator<A> modifier) {
        Object value = horizonAttachments(this).get(horizonKey(type));
        if (value == null) return;
        setAttached(type, modifier.apply(horizonCast(value)));
    }

    default <A> A setAttached(AttachmentType<A> type, A value) {
        Object previous = horizonAttachments(this).put(horizonKey(type), value);
        return horizonCast(previous);
    }

    default <A> A removeAttached(AttachmentType<A> type) {
        Object previous = horizonAttachments(this).remove(horizonKey(type));
        return horizonCast(previous);
    }
}
"#;

/// Bounded Fabric API `AttachmentRegistry`: `create()` starts a builder, and
/// `buildAndRegister` keeps one row per attachment id in a deterministic map
/// (codec marker, initializer marker, persistent flag, copy-on-death flag)
/// while answering the `AttachmentType` the mod stores. A second registration
/// for the same id fails, as the real registry's check does.
const FABRIC_ATTACHMENT_REGISTRY: &str = r#"package net.fabricmc.fabric.api.attachment.v1;

public final class AttachmentRegistry {
    private static final java.util.Map<String, Registration> HORIZON_REGISTRATIONS = new java.util.TreeMap<>();

    private AttachmentRegistry() {}

    public static <A> Builder<A> create() { return new Builder<>(); }

    /** The flags one registration retained, keyed by the attachment id. */
    public static final class Registration {
        private final String id;
        private final boolean initializer;
        private final boolean codec;
        private final boolean copyOnDeath;

        private Registration(String id, boolean initializer, boolean codec, boolean copyOnDeath) {
            this.id = id;
            this.initializer = initializer;
            this.codec = codec;
            this.copyOnDeath = copyOnDeath;
        }

        public String id() { return id; }
        public boolean hasInitializer() { return initializer; }
        /** Fabric keeps an attachment exactly when it was given a codec. */
        public boolean persistent() { return codec; }
        public boolean hasCodec() { return codec; }
        public boolean copyOnDeath() { return copyOnDeath; }
    }

    /** The registration retained for an attachment id, or null. */
    public static Registration horizonRegistration(String id) { return HORIZON_REGISTRATIONS.get(id); }

    /** Every registration, keyed by attachment id in id order. */
    public static java.util.Map<String, Registration> horizonRegistrations() { return new java.util.TreeMap<>(HORIZON_REGISTRATIONS); }

    public static final class Builder<A> {
        private java.util.function.Supplier<A> initializer;
        private {codec}<A> codec;
        private boolean copyOnDeath;

        private Builder() {}

        public Builder<A> initializer(java.util.function.Supplier<A> initializer) { this.initializer = initializer; return this; }

        public Builder<A> persistent({codec}<A> codec) { this.codec = codec; return this; }

        public Builder<A> copyOnDeath() { this.copyOnDeath = true; return this; }

        public AttachmentType<A> buildAndRegister({identifier} id) {
            if (id == null) throw new NullPointerException("id");
            String key = String.valueOf(id);
            if (HORIZON_REGISTRATIONS.containsKey(key)) throw new IllegalStateException("attachment already registered: " + key);
            HORIZON_REGISTRATIONS.put(key, new Registration(key, initializer != null, codec != null, copyOnDeath));
            return new AttachmentType<A>() {
                public {identifier} id() { return id; }
            };
        }
    }
}
"#;

// Fabric's and NeoForge's menu and screen-handler façades. A menu type is only
// retained (the factory and codec it was registered with, the id it answers
// for), the factory keeps the screen-opening data and display name a server
// writes, and no live menu or screen is opened here.
/// Fabric's extended menu type, under the 26.3 `menu/v1` name.
const FABRIC_MENU_TYPE: &str = "net/fabricmc/fabric/api/menu/v1/ExtendedMenuType";
const FABRIC_MENU_FACTORY: &str = "net/fabricmc/fabric/api/menu/v1/ExtendedMenuProvider";
/// The names 26.3 replaced, kept for a jar built against them: a pair lands
/// only for the name the target jar actually references.
const FABRIC_SCREEN_HANDLER_TYPE: &str =
    "net/fabricmc/fabric/api/screenhandler/v1/ExtendedScreenHandlerType";
const FABRIC_SCREEN_HANDLER_FACTORY: &str =
    "net/fabricmc/fabric/api/screenhandler/v1/ExtendedScreenHandlerFactory";

/// NeoForge's menu factories and the `MenuType` extension that keeps one
/// factory per menu id (`naturalId`).
const NEOFORGE_CONTAINER_FACTORY: &str = "net/neoforged/neoforge/network/IContainerFactory";
const NEOFORGE_EXTENDED_CONTAINER_FACTORY: &str =
    "net/neoforged/neoforge/network/IExtendedContainerFactory";
const NEOFORGE_MENU_TYPE_EXTENSION: &str =
    "net/neoforged/neoforge/common/extensions/IMenuTypeExtension";

/// Fabric's extended menu type: the constructor keeps the factory and the
/// codec marker it was handed, `create` routes to that factory, and the type it
/// wraps is the instance itself, so the `MenuType` a mod stores is the one it
/// registered. `{package}` and `{name}` carry the 26.3 and 1.20.x names.
const FABRIC_MENU_TYPE_SOURCE: &str = r#"package {package};

public class {name}<T extends {menu}, D> extends {menu_type} {
    private final ExtendedFactory<T, D> factory;
    private final {codec} codec;

    public {name}(ExtendedFactory<T, D> factory, {codec} codec) {
        // The façade's `MenuType` stub keeps the real class's two-argument
        // constructor; the feature flags it would carry are not modelled.
        super(null, null);
        this.factory = java.util.Objects.requireNonNull(factory, "menu factory cannot be null");
        this.codec = java.util.Objects.requireNonNull(codec, "codec cannot be null");
    }

    public T create(int windowId, {inventory} inventory, D data) { return factory.create(windowId, inventory, data); }

    /** The codec this registration retained: a marker, never bound to a buffer here. */
    public {codec} getCodec() { return codec; }

    public {codec} getStreamCodec() { return codec; }

    /** The game type this extended type is; the instance itself. */
    public {menu_type} getMenuType() { return this; }

    public interface ExtendedFactory<T extends {menu}, D> {
        T create(int windowId, {inventory} inventory, D data);
    }
}
"#;

/// Fabric's extended menu provider: the opening data its menu was handed plus
/// the display name a vanilla menu provider answers. The screen itself is the
/// mod's own, so the bounded accessor only names the client type.
const FABRIC_MENU_FACTORY_SOURCE: &str = r#"package {package};

public interface {name}<D> extends {menu_provider} {
    /** The data the server writes when it opens this menu. */
    D getScreenOpeningData({server_player} player);

    {component} getDisplayName();

    /** Bounded: the screen belongs to the mod, so none is built here. */
    default {screen} createScreen(int windowId, {inventory} inventory, {component} title) { return null; }
}
"#;

/// NeoForge's menu factory: one abstract method, so a mod's lambda or
/// anonymous class binds to the same descriptor the game calls.
const NEOFORGE_CONTAINER_FACTORY_SOURCE: &str = r#"package net.neoforged.neoforge.network;

public interface IContainerFactory<T extends {menu}> {
    T create(int windowId, {inventory} inventory, {byte_buf} data);

    /** The form the game calls when a menu was opened with no extra data. */
    default T create(int windowId, {inventory} inventory) { return create(windowId, inventory, null); }
}
"#;

/// The extended menu factory is the same single abstract method as
/// [`NEOFORGE_CONTAINER_FACTORY_SOURCE`]: the name is what a mod implements
/// when it writes the opening data itself, and adding a method would leave a
/// lambda without one to bind.
const NEOFORGE_EXTENDED_CONTAINER_FACTORY_SOURCE: &str = r#"package net.neoforged.neoforge.network;

public interface IExtendedContainerFactory<T extends {menu}> extends IContainerFactory<T> {
}
"#;

/// NeoForge's `MenuType` extension: `create` retains one factory per menu id
/// and answers the id a type was created under, while the game type keeps its
/// own constructor. The default `naturalId` is the marker the generated
/// `MenuType` stub answers through this interface.
const NEOFORGE_MENU_TYPE_EXTENSION_SOURCE: &str = r#"package net.neoforged.neoforge.common.extensions;

public interface IMenuTypeExtension {
    java.util.Map<String, net.neoforged.neoforge.network.IContainerFactory<?>> HORIZON_FACTORIES = new java.util.TreeMap<>();
    java.util.Map<Object, {identifier}> HORIZON_IDS = java.util.Collections.synchronizedMap(new java.util.WeakHashMap<Object, {identifier}>());

    static <T extends {menu}> {menu_type} create(net.neoforged.neoforge.network.IContainerFactory<T> factory) {
        return create(null, factory);
    }

    static <T extends {menu}> {menu_type} create({identifier} id, net.neoforged.neoforge.network.IContainerFactory<T> factory) {
        if (factory == null) throw new NullPointerException("factory");
        {menu_type} type = new {menu_type}(null, null) { };
        if (id != null) {
            HORIZON_FACTORIES.put(String.valueOf(id), factory);
            HORIZON_IDS.put(type, id);
        }
        return type;
    }

    /** The factory a menu id was created with, or null. */
    static net.neoforged.neoforge.network.IContainerFactory<?> horizonFactory(String id) { return HORIZON_FACTORIES.get(id); }

    /** Every retained factory, keyed by menu id in id order. */
    static java.util.Map<String, net.neoforged.neoforge.network.IContainerFactory<?>> horizonFactories() { return new java.util.TreeMap<>(HORIZON_FACTORIES); }

    /** Marker: the id this type was created under, or null when it named none. */
    default {identifier} naturalId() { return HORIZON_IDS.get(this); }
}
"#;

/// Bounded loader bootstrap values. They answer the queries entrypoint code
/// makes during startup (`getModContainer`, `getAllMods`, `getObjectShare`,
/// `isModLoaded`) without claiming to be the Fabric loader's registry: the
/// native host answers the same calls from real metadata.
const FABRIC_LOADER: &str = r#"package net.fabricmc.loader.api;

public final class FabricLoader {
    private static final FabricLoader HORIZON = new FabricLoader();
    private static final ModContainer HORIZON_MOD = ModContainer.horizon("minecraft", "26.3");
    private final ObjectShare horizonObjectShare = new ObjectShare();

    private FabricLoader() {}
    public static FabricLoader getInstance() { return HORIZON; }
    public <T> java.util.List<EntrypointContainer<T>> getEntrypointContainers(String key, Class<T> type) { return new java.util.ArrayList<>(); }
    public java.util.Optional<ModContainer> getModContainer(String modId) { return isModLoaded(modId) ? java.util.Optional.of(HORIZON_MOD) : java.util.Optional.empty(); }
    public java.util.Collection<ModContainer> getAllMods() { return java.util.Collections.singletonList(HORIZON_MOD); }
    public ObjectShare getObjectShare() { return horizonObjectShare; }
    public boolean isModLoaded(String modId) { return "minecraft".equals(modId) || "horizon".equals(modId); }
    public boolean isDevelopmentEnvironment() { return false; }
    public String getGameVersion() { return "26.3"; }
}
"#;

const FABRIC_MOD_CONTAINER: &str = r#"package net.fabricmc.loader.api;

import net.fabricmc.loader.api.metadata.ModMetadata;
import net.fabricmc.loader.api.metadata.Version;

public interface ModContainer {
    ModMetadata getMetadata();
    default String getModId() { return getMetadata().getId(); }
    default Version getVersion() { return getMetadata().getVersion(); }
    static ModContainer horizon(String id, String version) { return () -> ModMetadata.horizon(id, version); }
}
"#;

const FABRIC_ENTRYPOINT_CONTAINER: &str = r#"package net.fabricmc.loader.api;

public interface EntrypointContainer<T> {
    T getEntrypoint();
    ModContainer getProvider();
    Object getDefinition();
}
"#;

const FABRIC_OBJECT_SHARE: &str = r#"package net.fabricmc.loader.api;

public final class ObjectShare {
    private final java.util.Map<String, Object> horizonValues = new java.util.concurrent.ConcurrentHashMap<>();
    public Object get(String key) { return horizonValues.get(key); }
    @SuppressWarnings("unchecked") public <T> T get(Class<T> type, String key) { Object value = horizonValues.get(key); return type.isInstance(value) ? (T) value : null; }
    @SuppressWarnings("unchecked") public <T> T getOrDefault(String key, T fallback) { Object value = horizonValues.get(key); return value == null ? fallback : (T) value; }
    public Object put(String key, Object value) { return horizonValues.put(key, value); }
    public boolean containsKey(String key) { return horizonValues.containsKey(key); }
    public Object remove(String key) { return horizonValues.remove(key); }
    public java.util.Map<String, Object> asMap() { return java.util.Collections.unmodifiableMap(horizonValues); }
    public boolean isEmpty() { return horizonValues.isEmpty(); }
    public void clear() { horizonValues.clear(); }
}
"#;

const FABRIC_MOD_METADATA: &str = r#"package net.fabricmc.loader.api.metadata;

public interface ModMetadata {
    String getId();
    Version getVersion();
    String getName();
    String getDescription();
    static ModMetadata horizon(String id, String version) {
        return new ModMetadata() {
            public String getId() { return id; }
            public Version getVersion() { return Version.horizon(version); }
            public String getName() { return id; }
            public String getDescription() { return "Horizon bounded mod metadata"; }
        };
    }
}
"#;

const FABRIC_VERSION: &str = r#"package net.fabricmc.loader.api.metadata;

public interface Version {
    String getFriendlyString();
    static Version horizon(String version) { return () -> version; }
}
"#;

const FABRIC_LOADER_SHIMS: &[(&str, &str)] = &[
    ("net/fabricmc/loader/api/FabricLoader", FABRIC_LOADER),
    ("net/fabricmc/loader/api/ModContainer", FABRIC_MOD_CONTAINER),
    (
        "net/fabricmc/loader/api/EntrypointContainer",
        FABRIC_ENTRYPOINT_CONTAINER,
    ),
    ("net/fabricmc/loader/api/ObjectShare", FABRIC_OBJECT_SHARE),
    (
        "net/fabricmc/loader/api/metadata/ModMetadata",
        FABRIC_MOD_METADATA,
    ),
    ("net/fabricmc/loader/api/metadata/Version", FABRIC_VERSION),
];

/// Fabric's `fabric-datagen` entrypoint: Fabric calls this instead of
/// `onInitialize`, so the class only needs the one callback a datagen mod
/// implements.
const FABRIC_DATAGEN_ENTRYPOINT: &str = r#"package net.fabricmc.fabric.api.datagen.v1;

@FunctionalInterface
public interface DataGeneratorEntrypoint {
    void onInitializeDataGenerator(FabricDataGenerator fabricDataGenerator);
}
"#;

/// The bounded output a data provider receives: the mod container it generates
/// for and the folder its files would land in. Nothing is written here, so the
/// folder is only carried for the host's generator.
const FABRIC_DATA_OUTPUT: &str = r#"package net.fabricmc.fabric.api.datagen.v1;

import {mod_container};

public final class FabricDataOutput {
    private final {mod_container} horizonModContainer;
    private final java.nio.file.Path horizonOutputFolder;
    private final boolean horizonStrictValidation;

    public FabricDataOutput({mod_container} modContainer, java.nio.file.Path outputFolder, boolean strictValidation) {
        this.horizonModContainer = modContainer;
        this.horizonOutputFolder = outputFolder;
        this.horizonStrictValidation = strictValidation;
    }

    public {mod_container} getModContainer() { return horizonModContainer; }
    public java.nio.file.Path getOutputFolder() { return horizonOutputFolder; }
    public String getModId() { return horizonModContainer.getMetadata().getId(); }
    public boolean isStrictValidationEnabled() { return horizonStrictValidation; }
}
"#;

/// The bounded provider shape: an id and a kind the pack retains, and a `run`
/// the host owns. Every member is a default, so a provider whose own `run` has
/// the game's `run(CachedOutput)` descriptor does not collide with this
/// interface's no-op `run()`.
const FABRIC_DATA_PROVIDER: &str = r#"package net.fabricmc.fabric.api.datagen.v1.provider;

import net.fabricmc.fabric.api.datagen.v1.FabricDataOutput;

public interface DataProvider {
    /** The id this provider is retained under; the mod's own override wins. */
    default String getName() { return getClass().getSimpleName(); }

    /** The provider family, so the host can order one kind before another. */
    default String getKind() { return "provider"; }

    /** Generation is the host's work; adding a provider only retains it. */
    default void run() {}

    @FunctionalInterface
    interface Factory {
        DataProvider create(FabricDataOutput output);
    }
}
"#;

/// A recipe provider marker: it keeps the output field and the constructor a
/// ported provider calls, and answers its kind so the pack's ledger names it.
const FABRIC_RECIPE_PROVIDER: &str = r#"package net.fabricmc.fabric.api.datagen.v1.provider;

import net.fabricmc.fabric.api.datagen.v1.FabricDataOutput;

public abstract class FabricRecipeProvider implements DataProvider {
    protected final FabricDataOutput output;

    public FabricRecipeProvider(FabricDataOutput output, java.util.concurrent.CompletableFuture<?> registriesFuture) {
        this.output = output;
    }

    @Override
    public String getKind() { return "recipe"; }
}
"#;

/// The language provider marker. The key/value table a ported provider fills is
/// the host's, so the constructor only keeps the output and the mod id.
const FABRIC_LANGUAGE_PROVIDER: &str = r#"package net.fabricmc.fabric.api.datagen.v1.provider;

import net.fabricmc.fabric.api.datagen.v1.FabricDataOutput;

public abstract class FabricLanguageProvider implements DataProvider {
    protected final FabricDataOutput output;
    protected final String modId;

    public FabricLanguageProvider(FabricDataOutput output, String modId) {
        this.output = output;
        this.modId = modId;
    }

    @Override
    public String getKind() { return "language"; }
}
"#;

/// The model provider marker: same retained output, and no baker behind it.
const FABRIC_MODEL_PROVIDER: &str = r#"package net.fabricmc.fabric.api.datagen.v1.provider;

import net.fabricmc.fabric.api.datagen.v1.FabricDataOutput;

public abstract class FabricModelProvider implements DataProvider {
    protected final FabricDataOutput output;

    public FabricModelProvider(FabricDataOutput output) {
        this.output = output;
    }

    @Override
    public String getKind() { return "model"; }
}
"#;

/// The generator a datagen entrypoint is handed. `createPack` keeps the name the
/// pack is retained under (the mod id, or `resourcepacks/<id>` for a built-in
/// pack), and `Pack#addProvider` appends `{pack, id, kind, provider}` to one
/// ordered ledger the host reads back with `horizonProviders()`. Providers are
/// never run: datagen is a build-time step.
const FABRIC_DATA_GENERATOR: &str = r#"package net.fabricmc.fabric.api.datagen.v1;

import {identifier};
import {mod_container};
import net.fabricmc.fabric.api.datagen.v1.provider.DataProvider;

public final class FabricDataGenerator {
    private final FabricDataOutput horizonOutput;
    private final {mod_container} horizonModContainer;
    private final java.util.List<Object[]> horizonProviders = new java.util.ArrayList<>();

    public FabricDataGenerator(FabricDataOutput output, {mod_container} modContainer) {
        this.horizonOutput = output;
        this.horizonModContainer = modContainer;
    }

    public FabricDataOutput getOutput() { return horizonOutput; }
    public {mod_container} getModContainer() { return horizonModContainer; }
    public String getModId() { return horizonModContainer.getMetadata().getId(); }
    public boolean isStrictValidationEnabled() { return horizonOutput.isStrictValidationEnabled(); }

    public Pack createPack() {
        return new Pack(horizonModContainer.getMetadata().getName());
    }

    public Pack createBuiltinResourcePack({identifier} id) {
        return new Pack("resourcepacks/" + id);
    }

    /** One `{pack, id, kind, provider}` row per `addProvider`, in call order. */
    public java.util.List<Object[]> horizonProviders() {
        return java.util.Collections.unmodifiableList(new java.util.ArrayList<>(horizonProviders));
    }

    public final class Pack {
        private final String horizonPack;

        private Pack(String pack) { this.horizonPack = pack; }

        public Pack addProvider(DataProvider provider) {
            if (provider != null) {
                horizonProviders.add(new Object[] { horizonPack, provider.getName(), provider.getKind(), provider });
            }
            return this;
        }

        public Pack addProvider(DataProvider.Factory factory) {
            return factory == null ? this : addProvider(factory.create(horizonOutput));
        }

        public String getPackId() { return horizonPack; }
    }
}
"#;

/// The datagen slice, emitted as one unit (the generator names the pack and the
/// provider, the markers name the provider and the output). Each row carries the
/// roles its source names; the generator is the one that needs both, so a jar
/// without a resolvable id reports it rather than half a slice.
const FABRIC_DATAGEN_SHIMS: &[(&str, &str, &[&str])] = &[
    (
        "net/fabricmc/fabric/api/datagen/v1/DataGeneratorEntrypoint",
        FABRIC_DATAGEN_ENTRYPOINT,
        &[],
    ),
    (
        "net/fabricmc/fabric/api/datagen/v1/FabricDataOutput",
        FABRIC_DATA_OUTPUT,
        &["mod_container"],
    ),
    (
        "net/fabricmc/fabric/api/datagen/v1/provider/DataProvider",
        FABRIC_DATA_PROVIDER,
        &[],
    ),
    (
        "net/fabricmc/fabric/api/datagen/v1/provider/FabricRecipeProvider",
        FABRIC_RECIPE_PROVIDER,
        &[],
    ),
    (
        "net/fabricmc/fabric/api/datagen/v1/provider/FabricLanguageProvider",
        FABRIC_LANGUAGE_PROVIDER,
        &[],
    ),
    (
        "net/fabricmc/fabric/api/datagen/v1/provider/FabricModelProvider",
        FABRIC_MODEL_PROVIDER,
        &[],
    ),
    (
        "net/fabricmc/fabric/api/datagen/v1/FabricDataGenerator",
        FABRIC_DATA_GENERATOR,
        &["mod_container", "identifier"],
    ),
];

/// Bounded NeoForge event base. Result and cancellation are recorded so
/// bootstrap code can read them back; the host's bus owns ordering and the
/// listener dispatch itself, so nothing here posts or invokes gameplay.
const NEOFORGE_EVENT: &str = r#"package net.neoforged.bus.api;

public class Event {
    public enum Result { DENY, DEFAULT, ALLOW }

    private final boolean horizonCancelable;
    private boolean horizonCanceled;
    private Result horizonResult;

    public Event() { this(false); }
    public Event(boolean cancelable) { this.horizonCancelable = cancelable; }
    public boolean isCancelable() { return horizonCancelable; }
    public boolean isCanceled() { return horizonCanceled; }
    public void setCanceled(boolean canceled) { horizonCanceled = canceled; }
    public Result getResult() { return horizonResult == null ? Result.DEFAULT : horizonResult; }
    public void setResult(Result result) { horizonResult = result; }
    public boolean hasResult() { return horizonResult != null; }
}
"#;

/// `@SubscribeEvent` metadata on a method: the annotation is present so mod
/// listeners compile and can be inspected; the native host records the
/// listeners it actually dispatches.
const NEOFORGE_SUBSCRIBE_EVENT: &str = r#"package net.neoforged.bus.api;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.METHOD)
public @interface SubscribeEvent {
    EventPriority priority() default EventPriority.NORMAL;
    boolean receiveCancelled() default false;
    boolean receiveCanceled() default false;
}
"#;

/// NeoForge's key-mapping registration event. `register` retains the mappings
/// in order so bootstrap code can be inspected without a live options screen;
/// the host owns when the event is posted.
const NEOFORGE_REGISTER_KEY_MAPPINGS: &str = r#"package net.neoforged.neoforge.client.event;

import net.neoforged.bus.api.Event;
import {key_mapping};

public class RegisterKeyMappingsEvent extends Event {
    private final java.util.List<{key_mapping}> horizonKeyMappings = new java.util.ArrayList<>();

    public void register({key_mapping} binding) {
        if (binding != null) horizonKeyMappings.add(binding);
    }

    public java.util.List<{key_mapping}> horizonKeyMappings() {
        return java.util.Collections.unmodifiableList(horizonKeyMappings);
    }
}
"#;

/// NeoForge's client tick event and its two phases, as markers: the host posts
/// them and the listeners are the mod's own `@SubscribeEvent` methods, so the
/// façade only needs the type hierarchy.
const NEOFORGE_CLIENT_TICK_EVENT: &str = r#"package net.neoforged.neoforge.client.event;

import net.neoforged.bus.api.Event;

public abstract class ClientTickEvent extends Event {
    private ClientTickEvent() {}

    public static final class Pre extends ClientTickEvent {}
    public static final class Post extends ClientTickEvent {}
}
"#;

/// NeoForge's render-GUI event: it carries the frame's GUI value through to the
/// listener. Drawing is the host's work.
const NEOFORGE_RENDER_GUI_EVENT: &str = r#"package net.neoforged.neoforge.client.event;

import net.neoforged.bus.api.Event;
import {gui};

public class RenderGuiEvent extends Event {
    private final {gui} horizonGuiGraphics;

    protected RenderGuiEvent({gui} guiGraphics) { this.horizonGuiGraphics = guiGraphics; }

    public {gui} getGuiGraphics() { return horizonGuiGraphics; }

    public static final class Pre extends RenderGuiEvent {
        public Pre({gui} guiGraphics) { super(guiGraphics); }
    }

    public static final class Post extends RenderGuiEvent {
        public Post({gui} guiGraphics) { super(guiGraphics); }
    }
}
"#;

/// NeoForge's `ScreenEvent` and the `Init`/`Render` phases a listener takes.
/// They are markers: the host posts one per screen transition, each carrying
/// the screen it was posted for, and the phase classes only need the type
/// hierarchy plus the bounded `getScreen()` accessor.
const NEOFORGE_SCREEN_EVENT: &str = "net/neoforged/neoforge/client/event/ScreenEvent";
const NEOFORGE_SCREEN_EVENT_SOURCE: &str = r#"package net.neoforged.neoforge.client.event;

import net.neoforged.bus.api.Event;
import {screen};

public abstract class ScreenEvent extends Event {
    private final {screen} horizonScreen;

    protected ScreenEvent({screen} screen) { this.horizonScreen = screen; }

    public {screen} getScreen() { return horizonScreen; }

    public static class Init extends ScreenEvent {
        public Init({screen} screen) { super(screen); }
        public static class Pre extends Init { public Pre({screen} screen) { super(screen); } }
        public static final class Post extends Init { public Post({screen} screen) { super(screen); } }
    }

    public static class Render extends ScreenEvent {
        public Render({screen} screen) { super(screen); }
        public static class Pre extends Render { public Pre({screen} screen) { super(screen); } }
        public static final class Post extends Render { public Post({screen} screen) { super(screen); } }
    }
}
"#;

/// NeoForge's creative-tab contents event: the tab it is posted for and the
/// stacks a listener accepts, in call order. The real event also implements
/// `CreativeModeTab.Output`; the bounded shape keeps only `accept`/`getTab`,
/// which is what bootstrap code calls during registration.
const NEOFORGE_BUILD_CREATIVE_MODE_TAB_CONTENTS: &str = r#"package net.neoforged.neoforge.event;

import net.neoforged.bus.api.Event;
import {creative_mode_tab};
import {item_stack};

public class BuildCreativeModeTabContentsEvent extends Event {
    private final {creative_mode_tab} horizonTab;
    private final java.util.List<{item_stack}> horizonEntries = new java.util.ArrayList<>();

    public BuildCreativeModeTabContentsEvent({creative_mode_tab} tab) { this.horizonTab = tab; }

    public {creative_mode_tab} getTab() { return horizonTab; }

    public void accept({item_stack} stack) {
        if (stack != null) horizonEntries.add(stack);
    }
    public java.util.List<{item_stack}> horizonEntries() {
        return java.util.Collections.unmodifiableList(horizonEntries);
    }
}
"#;

/// NeoForge's `EntityRenderersEvent$RegisterRenderers`: the nested class a
/// `@SubscribeEvent` method takes, retaining what it registers. Rendering is
/// the host's work, so there is no renderer value here — only the pairs.
const NEOFORGE_ENTITY_RENDERERS: &str = r#"package net.neoforged.neoforge.client.event;

import net.neoforged.bus.api.Event;
import {block_entity_renderer_provider};
import {block_entity_type};
import {entity_renderer_provider};
import {entity_type};

public class EntityRenderersEvent extends Event {
    private EntityRenderersEvent() {}

    public static final class RegisterRenderers extends EntityRenderersEvent {
        private final java.util.List<Object[]> horizonEntityRenderers = new java.util.ArrayList<>();
        private final java.util.List<Object[]> horizonBlockEntityRenderers = new java.util.ArrayList<>();

        public RegisterRenderers() {}
        public void registerEntityRenderer({entity_type} type, {entity_renderer_provider} provider) {
            if (type != null && provider != null) horizonEntityRenderers.add(new Object[] { type, provider });
        }

        public void registerBlockEntityRenderer({block_entity_type} type, {block_entity_renderer_provider} provider) {
            if (type != null && provider != null) horizonBlockEntityRenderers.add(new Object[] { type, provider });
        }

        /** `{type, provider}` pairs in registration order. */
        public java.util.List<Object[]> horizonEntityRenderers() {
            return java.util.Collections.unmodifiableList(new java.util.ArrayList<>(horizonEntityRenderers));
        }

        public java.util.List<Object[]> horizonBlockEntityRenderers() {
            return java.util.Collections.unmodifiableList(new java.util.ArrayList<>(horizonBlockEntityRenderers));
        }
    }
}
"#;

/// NeoForge's `ModelEvent` and its two registration nested classes. The model
/// event is a library class (emitted only when the real one is absent):
/// `RegisterAdditional` retains the model keys a listener asks the bakery for,
/// and `ModifyBakingResult` the baked models it hands back, both in call order.
/// 26.3 keys a model by its `ModelState`; the role resolves the name the jar
/// carries, so an older jar binds through `ModelResourceLocation`.
const NEOFORGE_MODEL_EVENT: &str = "net/neoforged/neoforge/client/event/ModelEvent";
const NEOFORGE_MODEL_EVENT_SOURCE: &str = r#"package net.neoforged.neoforge.client.event;

import net.neoforged.bus.api.Event;
import {baked_model};
import {model_key};

public class ModelEvent extends Event {
    private ModelEvent() {}
    public static class RegisterAdditional extends ModelEvent {
        private final java.util.List<{model_key}> horizonModels = new java.util.ArrayList<>();
        public RegisterAdditional() {}
        public void register({model_key} location) {
            if (location != null) horizonModels.add(location);
        }
        /** The registered model keys, in call order. */
        public java.util.List<{model_key}> horizonModels() {
            return java.util.Collections.unmodifiableList(new java.util.ArrayList<>(horizonModels));
        }
    }
    public static class ModifyBakingResult extends ModelEvent {
        private final java.util.Map<{model_key}, {baked_model}> horizonModels = new java.util.LinkedHashMap<>();
        public ModifyBakingResult() {}
        /** The baked models the host supplied, in insertion order. */
        public java.util.Map<{model_key}, {baked_model}> getModels() {
            return java.util.Collections.unmodifiableMap(new java.util.LinkedHashMap<>(horizonModels));
        }

        public void horizonRegister({model_key} location, {baked_model} model) {
            if (location != null && model != null) horizonModels.put(location, model);
        }
    }
}
"#;

/// NeoForge's attachment type: a library class, emitted only when the real
/// `net.neoforged.neoforge.attachment.AttachmentType` is absent.
const NEOFORGE_ATTACHMENT_TYPE: &str = "net/neoforged/neoforge/attachment/AttachmentType";

const NEOFORGE_ENTITY_JOIN_LEVEL_EVENT: &str =
    "net/neoforged/neoforge/event/entity/EntityJoinLevelEvent";
const NEOFORGE_REGISTER_CAPABILITIES_EVENT: &str =
    "net/neoforged/neoforge/capabilities/RegisterCapabilitiesEvent";
// NeoForge 26.3 puts `BiomeModifier` in `common.world`, next to
// `ModifiableBiomeInfo`, not in a `common.world.biome` package.
const NEOFORGE_BIOME_MODIFIER: &str = "net/neoforged/neoforge/common/world/BiomeModifier";
// Legacy marker: 26.3 NeoForge has no `IStorage` (its transfer API is
// `net.neoforged.neoforge.transfer.ResourceHandler`). Keep the name for jars
// built against an older API without claiming a 26.3 class.
const NEOFORGE_ISTORAGE: &str = "net/neoforged/neoforge/common/capabilities/IStorage";

const NEOFORGE_ENTITY_JOIN_LEVEL_EVENT_SOURCE: &str = r#"package net.neoforged.neoforge.event.entity;
public class EntityJoinLevelEvent extends net.neoforged.bus.api.Event {
    private final net.minecraft.world.entity.Entity horizonEntity;
    private final net.minecraft.world.level.Level horizonLevel;
    private final boolean horizonLoadedFromDisk;
    public EntityJoinLevelEvent(net.minecraft.world.entity.Entity entity, net.minecraft.world.level.Level level) { this(entity, level, false); }
    public EntityJoinLevelEvent(net.minecraft.world.entity.Entity entity, net.minecraft.world.level.Level level, boolean loadedFromDisk) { super(true); this.horizonEntity = entity; this.horizonLevel = level; this.horizonLoadedFromDisk = loadedFromDisk; }
    public net.minecraft.world.entity.Entity getEntity() { return horizonEntity; }
    public net.minecraft.world.level.Level getLevel() { return horizonLevel; }
    public boolean loadedFromDisk() { return horizonLoadedFromDisk; }
}
"#;

const NEOFORGE_REGISTER_CAPABILITIES_EVENT_SOURCE: &str = r#"package net.neoforged.neoforge.capabilities;
public class RegisterCapabilitiesEvent extends net.neoforged.bus.api.Event {
    private final java.util.List<Object> horizonRegistrations = new java.util.ArrayList<>();
    private void horizonKeep(Object capability, Object target, Object provider) { horizonRegistrations.add(new Object[] { capability, target, provider }); }
    public <T, C> void registerBlock(net.neoforged.neoforge.capabilities.BlockCapability<T, C> capability, net.neoforged.neoforge.capabilities.BlockCapabilityCache.Provider<T, C> provider, net.minecraft.world.level.block.Block... blocks) { horizonKeep(capability, blocks, provider); }
    public <T, C> void registerBlockEntity(net.neoforged.neoforge.capabilities.BlockCapability<T, C> capability, net.minecraft.world.level.block.entity.BlockEntityType type, net.neoforged.neoforge.capabilities.BlockCapabilityCache.Provider<T, C> provider) { horizonKeep(capability, type, provider); }
    public <T, C> void registerItem(net.neoforged.neoforge.capabilities.ItemCapability<T, C> capability, net.neoforged.neoforge.capabilities.ItemCapabilityCache.Provider<T, C> provider, net.minecraft.world.item.Item... items) { horizonKeep(capability, items, provider); }
    public <T, C> void registerEntity(net.neoforged.neoforge.capabilities.EntityCapability<T, C> capability, net.neoforged.neoforge.capabilities.EntityCapabilityCache.Provider<T, C> provider, net.minecraft.world.entity.EntityType... types) { horizonKeep(capability, types, provider); }
    public boolean isBlockRegistered(net.neoforged.neoforge.capabilities.BlockCapability<?, ?> capability, net.minecraft.world.level.block.Block block) { return false; }
    public boolean isEntityRegistered(net.neoforged.neoforge.capabilities.EntityCapability<?, ?> capability, net.minecraft.world.entity.EntityType type) { return false; }
    public boolean isItemRegistered(net.neoforged.neoforge.capabilities.ItemCapability<?, ?> capability, net.minecraft.world.item.Item item) { return false; }
    public void setProxyable(net.neoforged.neoforge.capabilities.Capability<?> capability) {}
    public void setNonProxyable(net.neoforged.neoforge.capabilities.Capability<?> capability) {}
    public java.util.List<Object> horizonRegistrations() { return java.util.Collections.unmodifiableList(horizonRegistrations); }
}
"#;

const NEOFORGE_BIOME_MODIFIER_SOURCE: &str = r#"package net.neoforged.neoforge.common.world;
public interface BiomeModifier {
    default void modify(net.minecraft.core.RegistryAccess registryAccess, net.minecraft.core.Holder<net.minecraft.world.level.biome.Biome> biome, BiomeModifier.Phase phase, net.minecraft.world.level.biome.BiomeGenerationSettings.Builder builder) {}
    default void modify(net.minecraft.core.Holder<net.minecraft.world.level.biome.Biome> biome, BiomeModifier.Phase phase, net.minecraft.world.level.biome.BiomeGenerationSettings.Builder builder) { modify(null, biome, phase, builder); }
    default Object codec() { return null; }
    enum Phase { BEFORE_EVERYTHING, ADD, REMOVE, MODIFY, AFTER_EVERYTHING }
}
"#;
const NEOFORGE_BIOME_MODIFIER_OBJECT_SOURCE: &str = r#"package net.neoforged.neoforge.common.world;
public interface BiomeModifier {
    default void modify(Object registryAccess, Object biome, BiomeModifier.Phase phase, Object builder) {}
    default Object codec() { return null; }
    enum Phase { BEFORE_EVERYTHING, ADD, REMOVE, MODIFY, AFTER_EVERYTHING }
}
"#;

// Legacy marker, kept object-shaped on purpose: NeoForge's `IStorage` (a
// capability shape from before the 26.3 transfer API) names `Capability<T>`
// from `net.neoforged.neoforge.common.capabilities`, `Direction` and
// `CompoundTag`. The first is not a 26.3 class at all, so typing these
// parameters would make the shim uncompilable for the very jars it serves; a
// caller sees the bound through `Report::legacy` instead.
const NEOFORGE_ISTORAGE_SOURCE: &str = r#"package net.neoforged.neoforge.common.capabilities;
public interface IStorage<T> {
    default Object readNBT(Object capability, T instance, Object side) { return null; }
    default void writeNBT(Object capability, T instance, Object side, Object nbt) {}
}
"#;

/// Legacy Forge's channel factory. `newSimpleChannel` takes `ResourceLocation`
/// (renamed `Identifier` in 26.3) and `NetworkProtocolVersion` (a Forge API
/// type, not a game class), so every parameter stays `Object`: the shim keeps
/// a *source* call linkable, and a jar precompiled against real Forge is
/// reported through `Report::legacy` rather than failing silently.
const LEGACY_FORGE_NETWORK_REGISTRY_SOURCE: &str = concat!(
    "package net.minecraftforge.network;\n",
    "import net.minecraftforge.network.simple.SimpleChannel;\n",
    "public final class NetworkRegistry {\n",
    "    private NetworkRegistry() {}\n",
    "    public static SimpleChannel newSimpleChannel(Object name, Object protocol, Object clientAccepted, Object serverAccepted) { return new SimpleChannel(); }\n",
    "}\n",
);

/// Legacy Forge's channel. The encoder/decoder/handler carry the JDK types a
/// compiled call site erases to (`BiFunction`/`BiConsumer`); the message id and
/// decoder remain opaque markers, and byte encoding plus sends are still
/// outside this boundary.
const LEGACY_FORGE_SIMPLE_CHANNEL_SOURCE: &str = concat!(
    "package net.minecraftforge.network.simple;\n",
    "public class SimpleChannel {\n",
    "    public SimpleChannel registerMessage(int id, Class type, java.util.function.BiFunction encoder, java.util.function.BiFunction decoder, java.util.function.BiConsumer handler) { return this; }\n",
    "}\n",
);

/// Bounded NeoForge `AttachmentType`: `builder(Supplier)` keeps the default
/// supplier, `serialize(Codec, Supplier)` the codec and its default,
/// `copyOnDeath()` the flag, and `build()` the value. NeoForge registers the
/// type from a `DeferredRegister`, so the bounded marker here is
/// `horizonRegister(Identifier)`: it retains the id it is given, and the host
/// reads the rows back through `horizonRegistrations()`.
const NEOFORGE_ATTACHMENT_TYPE_SOURCE: &str = r#"package net.neoforged.neoforge.attachment;

public final class AttachmentType<T> {
    private static final java.util.Map<String, AttachmentType<?>> HORIZON_REGISTRATIONS = new java.util.TreeMap<>();

    private final java.util.function.Supplier<T> horizonDefault;
    private final {codec}<T> horizonCodec;
    private final boolean horizonCopyOnDeath;
    private {identifier} horizonId;

    private AttachmentType(java.util.function.Supplier<T> defaultValue, {codec}<T> codec, boolean copyOnDeath) {
        this.horizonDefault = defaultValue;
        this.horizonCodec = codec;
        this.horizonCopyOnDeath = copyOnDeath;
    }

    public static <T> Builder<T> builder(java.util.function.Supplier<T> defaultValueSupplier) { return new Builder<>(defaultValueSupplier); }

    /** The registered value type's default: null when the builder kept none. */
    public T defaultValue() { return horizonDefault == null ? null : horizonDefault.get(); }

    public boolean hasCodec() { return horizonCodec != null; }
    public boolean persistent() { return horizonCodec != null; }
    public boolean copyOnDeath() { return horizonCopyOnDeath; }

    /** Retains the id this attachment is registered under (the bounded NeoForge registration marker). */
    public AttachmentType<T> horizonRegister({identifier} id) {
        if (id == null) throw new NullPointerException("id");
        String key = String.valueOf(id);
        if (HORIZON_REGISTRATIONS.containsKey(key)) throw new IllegalStateException("attachment already registered: " + key);
        this.horizonId = id;
        HORIZON_REGISTRATIONS.put(key, this);
        return this;
    }

    /** The id retained by `horizonRegister`, or null. */
    public {identifier} horizonId() { return horizonId; }

    public static AttachmentType<?> horizonRegistered(String id) { return HORIZON_REGISTRATIONS.get(id); }

    public static java.util.Map<String, AttachmentType<?>> horizonRegistrations() { return new java.util.TreeMap<>(HORIZON_REGISTRATIONS); }

    public static final class Builder<T> {
        private java.util.function.Supplier<T> horizonDefault;
        private {codec}<T> horizonCodec;
        private boolean horizonCopyOnDeath;

        private Builder(java.util.function.Supplier<T> defaultValueSupplier) { this.horizonDefault = defaultValueSupplier; }

        /** Keeps the codec marker; the supplied default overrides the builder's. */
        public Builder<T> serialize({codec}<T> codec, java.util.function.Supplier<T> defaultSupplier) {
            this.horizonCodec = codec;
            if (defaultSupplier != null) this.horizonDefault = defaultSupplier;
            return this;
        }

        public Builder<T> copyOnDeath() { this.horizonCopyOnDeath = true; return this; }

        public AttachmentType<T> build() { return new AttachmentType<>(horizonDefault, horizonCodec, horizonCopyOnDeath); }
    }
}
"#;

/// Legacy Forge keeps the same result/cancellation surface in its own package.
const FORGE_EVENT: &str = r#"package net.minecraftforge.eventbus.api;

public class Event {
    public enum Result { DENY, DEFAULT, ALLOW }

    private boolean horizonCanceled;
    private Result horizonResult;

    public boolean isCancelable() { return true; }
    public boolean isCanceled() { return horizonCanceled; }
    public void setCanceled(boolean canceled) { horizonCanceled = canceled; }
    public Result getResult() { return horizonResult == null ? Result.DEFAULT : horizonResult; }
    public void setResult(Result result) { horizonResult = result; }
    public boolean hasResult() { return horizonResult != null; }
}
"#;

/// The game types the shim signatures name (client HUD/tick/tooltip and the
/// item-group/renderer registration façades), in 26.3 preference order (26.3
/// renamed `GuiGraphics` to `GuiGraphicsExtractor` and `TexturedModelData` to
/// `LayerDefinition`). The first candidate the game jar carries wins; a
/// server-shaped jar carries none of the client ones, and the shims that need a
/// role are then skipped.
const CLIENT_TYPE_CANDIDATES: &[(&str, &[&str])] = &[
    (
        "gui",
        &[
            "net/minecraft/client/gui/GuiGraphicsExtractor",
            "net/minecraft/client/gui/GuiGraphics",
            "net/minecraft/client/gui/DrawContext",
        ],
    ),
    (
        "ticks",
        &[
            "net/minecraft/client/render/RenderTickCounter",
            "net/minecraft/client/gui/render/RenderTickCounter",
            "net/minecraft/client/DeltaTracker",
        ],
    ),
    (
        "minecraft",
        &[
            "net/minecraft/client/Minecraft",
            "net/minecraft/client/MinecraftClient",
        ],
    ),
    ("key_mapping", &["net/minecraft/client/KeyMapping"]),
    (
        "item_stack",
        &[
            "net/minecraft/world/item/ItemStack",
            "net/minecraft/item/ItemStack",
        ],
    ),
    (
        "item",
        &["net/minecraft/world/item/Item", "net/minecraft/item/Item"],
    ),
    (
        "block",
        &[
            "net/minecraft/world/level/block/Block",
            "net/minecraft/block/Block",
        ],
    ),
    (
        "block_state",
        &[
            "net/minecraft/world/level/block/state/BlockState",
            "net/minecraft/block/BlockState",
        ],
    ),
    (
        "block_pos",
        &[
            "net/minecraft/core/BlockPos",
            "net/minecraft/util/math/BlockPos",
        ],
    ),
    (
        "block_and_tint_getter",
        &[
            "net/minecraft/world/level/BlockAndTintGetter",
            "net/minecraft/world/world/BlockRenderView",
        ],
    ),
    (
        "tooltip_context",
        &[
            "net/minecraft/world/item/Item$TooltipContext",
            "net/minecraft/item/Item$TooltipContext",
        ],
    ),
    (
        "tooltip_type",
        &[
            "net/minecraft/world/item/TooltipFlag",
            "net/minecraft/item/tooltip/TooltipType",
            "net/minecraft/world/inventory/tooltip/TooltipType",
            "net/minecraft/client/item/TooltipContext",
        ],
    ),
    (
        "component",
        &[
            "net/minecraft/network/chat/Component",
            "net/minecraft/text/Text",
        ],
    ),
    // The client screen slice's roles. A jar built against the older package
    // (`gui/screen/...`) still binds through the second candidate.
    (
        "screen",
        &[
            "net/minecraft/client/gui/screens/Screen",
            "net/minecraft/client/gui/screen/Screen",
        ],
    ),
    (
        "abstract_container_screen",
        &[
            "net/minecraft/client/gui/screens/inventory/AbstractContainerScreen",
            "net/minecraft/client/gui/screen/ingame/AbstractContainerScreen",
        ],
    ),
    (
        "clickable_widget",
        &[
            "net/minecraft/client/gui/components/ClickableWidget",
            "net/minecraft/client/gui/components/AbstractWidget",
        ],
    ),
    (
        "creative_mode_tab",
        &["net/minecraft/world/item/CreativeModeTab"],
    ),
    (
        "feature_flag_set",
        &["net/minecraft/world/flag/FeatureFlagSet"],
    ),
    ("resource_key", &["net/minecraft/resources/ResourceKey"]),
    ("entity_type", &["net/minecraft/world/entity/EntityType"]),
    (
        "block_entity_type",
        &["net/minecraft/world/level/block/entity/BlockEntityType"],
    ),
    (
        "entity_renderer_provider",
        &["net/minecraft/client/renderer/entity/EntityRendererProvider"],
    ),
    (
        "block_entity_renderer_provider",
        &["net/minecraft/client/renderer/blockentity/BlockEntityRendererProvider"],
    ),
    (
        "model_layer_location",
        &["net/minecraft/client/model/geom/ModelLayerLocation"],
    ),
    // 26.3 renamed `TexturedModelData`; the old name stays as the fallback so a
    // jar compiled against it still resolves.
    (
        "textured_model_data",
        &[
            "net/minecraft/client/model/geom/builders/LayerDefinition",
            "net/minecraft/client/model/geom/builders/TexturedModelData",
        ],
    ),
    // The client model-loading slice's roles. 26.3 moved `ModelState` to the
    // blockstate dispatch package and replaced `ModelResourceLocation` with it,
    // so the names it replaced stay last and a jar built against them binds.
    (
        "identifier",
        &[
            "net/minecraft/resources/Identifier",
            "net/minecraft/resources/ResourceLocation",
        ],
    ),
    (
        "unbaked_model",
        &["net/minecraft/client/resources/model/UnbakedModel"],
    ),
    (
        "baked_model",
        &["net/minecraft/client/resources/model/BakedModel"],
    ),
    (
        "model_loader",
        &["net/minecraft/client/resources/model/ModelLoader"],
    ),
    (
        "model_key",
        &[
            "net/minecraft/client/renderer/block/dispatch/ModelState",
            "net/minecraft/client/resources/model/ModelState",
            "net/minecraft/client/resources/model/ModelResourceLocation",
        ],
    ),
];

/// Resolve the client roles this game jar carries. A type the mod itself names
/// wins over the table order, so an explicitly typed listener (`public void
/// getTooltip(ItemStack stack, Item.TooltipContext context, TooltipFlag flag,
/// List<Component> lines)`) still overrides the signature it was compiled
/// against.
fn resolve_client_types(
    game: &BTreeMap<String, Class>,
    lib_names: &BTreeSet<String>,
    referenced: &BTreeSet<String>,
) -> BTreeMap<&'static str, String> {
    let mut out = BTreeMap::new();
    for (role, candidates) in CLIENT_TYPE_CANDIDATES {
        let present = |name: &str| game.contains_key(name) && !lib_names.contains(name);
        let picked = candidates
            .iter()
            .copied()
            .find(|name| referenced.contains(*name) && present(name))
            .or_else(|| candidates.iter().copied().find(|name| present(name)));
        if let Some(name) = picked {
            out.insert(*role, name.to_owned());
        }
    }
    out
}

/// The game and dependency types an attachment registration names. 26.3 calls
/// its id `Identifier`; `ResourceLocation` stays second so a jar compiled
/// against the name it replaced still resolves. `Codec` lives in the game's
/// serialization dependency, so it is a library class here rather than a game
/// one. `Supplier` is JDK and stays a literal in the templates.
const ATTACHMENT_TYPE_CANDIDATES: &[(&str, &[&str])] = &[
    (
        "identifier",
        &[
            "net/minecraft/resources/Identifier",
            "net/minecraft/resources/ResourceLocation",
        ],
    ),
    ("codec", &["com/mojang/serialization/Codec"]),
];

/// Resolve the attachment roles this classpath carries. A game class is seeded
/// into the façade so the shim compiles against the real stub; a dependency
/// class (a library jar, or one the target jars bundle) is already on `javac`'s
/// classpath. A role none of them carries stays unresolved, and the shim that
/// needs it is reported (`emit_role_shim`).
fn resolve_attachment_types(
    game: &BTreeMap<String, Class>,
    lib_names: &BTreeSet<String>,
    own: &BTreeSet<&str>,
    wanted: &mut BTreeSet<String>,
) -> BTreeMap<&'static str, String> {
    let mut out = BTreeMap::new();
    for (role, candidates) in ATTACHMENT_TYPE_CANDIDATES {
        let picked = candidates.iter().copied().find(|name| {
            game.contains_key(*name) || lib_names.contains(*name) || own.contains(*name)
        });
        if let Some(name) = picked {
            if game.contains_key(name) && !wanted.contains(name) {
                wanted.insert(name.to_owned());
            }
            out.insert(*role, name.to_owned());
        }
    }
    out
}

/// The game and dependency types a menu registration names. 26.3 renamed the
/// id to `Identifier`, the buffer to `RegistryFriendlyByteBuf` and the codec to
/// `StreamCodec`; the names they replaced stay second so a jar compiled against
/// them still binds. `screen` is the client GUI a factory's menu opens, so a
/// server-shaped jar leaves it unresolved and the factory is reported.
const MENU_TYPE_CANDIDATES: &[(&str, &[&str])] = &[
    (
        "menu",
        &["net/minecraft/world/inventory/AbstractContainerMenu"],
    ),
    ("menu_type", &["net/minecraft/world/inventory/MenuType"]),
    ("menu_provider", &["net/minecraft/world/MenuProvider"]),
    (
        "inventory",
        &["net/minecraft/world/entity/player/Inventory"],
    ),
    (
        "byte_buf",
        &[
            "net/minecraft/network/RegistryFriendlyByteBuf",
            "net/minecraft/network/FriendlyByteBuf",
        ],
    ),
    (
        "codec",
        &[
            "net/minecraft/network/codec/StreamCodec",
            "net/minecraft/network/codec/PacketCodec",
            "com/mojang/serialization/Codec",
        ],
    ),
    (
        "identifier",
        &[
            "net/minecraft/resources/Identifier",
            "net/minecraft/resources/ResourceLocation",
        ],
    ),
    (
        "component",
        &[
            "net/minecraft/network/chat/Component",
            "net/minecraft/text/Text",
        ],
    ),
    (
        "server_player",
        &["net/minecraft/server/level/ServerPlayer"],
    ),
    (
        "screen",
        &[
            "net/minecraft/client/gui/screens/Screen",
            "net/minecraft/client/gui/screen/Screen",
        ],
    ),
];

/// Resolve the menu roles this classpath carries: the game jar first, then the
/// dependency jars, then the target's own classes. A game class is seeded into
/// the façade so the shim compiles against the real stub, while a dependency
/// class is already on `javac`'s classpath. A role none of them carries stays
/// unresolved and the shim that needs it is reported (`emit_role_shim`).
fn resolve_menu_types(
    game: &BTreeMap<String, Class>,
    lib_names: &BTreeSet<String>,
    own: &BTreeSet<&str>,
    wanted: &mut BTreeSet<String>,
) -> BTreeMap<&'static str, String> {
    let mut out = BTreeMap::new();
    for (role, candidates) in MENU_TYPE_CANDIDATES {
        let picked = candidates.iter().copied().find(|name| {
            game.contains_key(*name) || lib_names.contains(*name) || own.contains(*name)
        });
        if let Some(name) = picked {
            if game.contains_key(name) && !wanted.contains(name) {
                wanted.insert(name.to_owned());
            }
            out.insert(*role, name.to_owned());
        }
    }
    out
}

/// The types a Fabric datagen registration names: the loader's `ModContainer`
/// (a dependency class, or the façade's own loader slice when that unit is
/// emitted) and the game's id. 26.3 calls the id `Identifier`; the name it
/// replaced stays second, so a jar built against it still binds.
const DATAGEN_TYPE_CANDIDATES: &[(&str, &[&str])] = &[
    ("mod_container", &["net/fabricmc/loader/api/ModContainer"]),
    (
        "identifier",
        &[
            "net/minecraft/resources/Identifier",
            "net/minecraft/resources/ResourceLocation",
        ],
    ),
];

/// Resolve the datagen roles this classpath carries. `loader_shim` says the
/// loader unit lands in this same compilation, which is where `ModContainer`
/// comes from for a jar-only slice. A game class is seeded into the façade so
/// the shim compiles against the real stub; a role that resolves nowhere stays
/// unresolved and the slice is reported instead of being emitted.
fn resolve_datagen_types(
    game: &BTreeMap<String, Class>,
    lib_names: &BTreeSet<String>,
    own: &BTreeSet<&str>,
    loader_shim: bool,
    wanted: &mut BTreeSet<String>,
) -> BTreeMap<&'static str, String> {
    let mut out = BTreeMap::new();
    for (role, candidates) in DATAGEN_TYPE_CANDIDATES {
        let picked = candidates.iter().copied().find(|name| {
            game.contains_key(*name)
                || lib_names.contains(*name)
                || own.contains(*name)
                || (loader_shim && *name == "net/fabricmc/loader/api/ModContainer")
        });
        if let Some(name) = picked {
            if game.contains_key(name) && !wanted.contains(name) {
                wanted.insert(name.to_owned());
            }
            out.insert(*role, name.to_owned());
        }
    }
    out
}

/// `{gui}` → the resolved class's Java source name.
fn fill_types(template: &str, types: &BTreeMap<&'static str, String>) -> String {
    types
        .iter()
        .fold(template.to_owned(), |source, (role, name)| {
            source.replace(&format!("{{{role}}}"), &java_name(name))
        })
}

/// Writes the API-only shims: a real API class is never shadowed, and a shim
/// whose signature names a game role this jar cannot resolve is reported
/// instead of emitted.
struct ClientShims<'a> {
    src: &'a Path,
    available: &'a BTreeSet<String>,
    types: &'a BTreeMap<&'static str, String>,
}

impl ClientShims<'_> {
    fn emit(
        &self,
        files: &mut Vec<PathBuf>,
        report: &mut Report,
        class: &str,
        requires: &[&str],
        source: impl FnOnce() -> String,
    ) -> Result<bool, String> {
        if self.available.contains(class) {
            return Ok(false);
        }
        let missing: Vec<&str> = requires
            .iter()
            .copied()
            .filter(|role| !self.types.contains_key(role))
            .collect();
        if !missing.is_empty() {
            report.missing.push(format!(
                "{class} (needs client type {})",
                missing.join(", ")
            ));
            return Ok(false);
        }
        write_shim(files, self.src, class, &source())?;
        Ok(true)
    }
}

/// Emits one Fabric networking shim: a real API class is never shadowed, and a
/// shim whose signature names a game class this jar does not carry is reported
/// instead of emitted with an invented type.
fn emit_networking_shim(
    files: &mut Vec<PathBuf>,
    report: &mut Report,
    src: &Path,
    available: &BTreeSet<String>,
    game: &BTreeMap<String, Class>,
    class: &str,
    requires: &[&str],
    source: impl FnOnce() -> String,
) -> Result<(), String> {
    if available.contains(class) {
        return Ok(());
    }
    let missing: Vec<&str> = requires
        .iter()
        .copied()
        .filter(|ty| !game.contains_key(*ty))
        .collect();
    if !missing.is_empty() {
        report
            .missing
            .push(format!("{class} (needs {})", missing.join(", ")));
        return Ok(());
    }
    write_shim(files, src, class, &source())
}

/// Emits one API-only shim whose signature names roles resolved from the game
/// jar and the dependency jars (the attachment slice). A real API class is
/// never shadowed, and a role this jar cannot resolve is reported instead of
/// being emitted with an invented type. Answers whether a shim was written.
fn emit_role_shim(
    files: &mut Vec<PathBuf>,
    report: &mut Report,
    src: &Path,
    available: &BTreeSet<String>,
    types: &BTreeMap<&'static str, String>,
    class: &str,
    requires: &[&str],
    source: impl FnOnce() -> String,
) -> Result<bool, String> {
    if available.contains(class) {
        return Ok(false);
    }
    let missing: Vec<&str> = requires
        .iter()
        .copied()
        .filter(|role| !types.contains_key(role))
        .collect();
    if !missing.is_empty() {
        report
            .missing
            .push(format!("{class} (needs {})", missing.join(", ")));
        return Ok(false);
    }
    write_shim(files, src, class, &source())?;
    Ok(true)
}

/// Writes one generated shim and records it for the `javac` source list.
fn write_shim(
    files: &mut Vec<PathBuf>,
    src: &Path,
    class: &str,
    source: &str,
) -> Result<(), String> {
    let file = src.join(format!("{class}.java"));
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::write(&file, source).map_err(|e| format!("{}: {e}", file.display()))?;
    files.push(file);
    Ok(())
}

fn tree(
    name: &str,
    top: bool,
    game: &BTreeMap<String, Class>,
    available: &BTreeSet<String>,
    used: &BTreeMap<String, BTreeSet<(String, String)>>,
    nested: &BTreeMap<String, BTreeSet<String>>,
    mixin_impls: &BTreeMap<String, Vec<MixinImpl>>,
    report: &mut Report,
) -> String {
    let inner: Vec<String> = nested
        .get(name)
        .map(|children| {
            children
                .iter()
                .map(|child| {
                    tree(
                        &format!("{name}${child}"),
                        false,
                        game,
                        available,
                        used,
                        nested,
                        mixin_impls,
                        report,
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    match game.get(name) {
        Some(class) => render(
            class,
            used.get(name),
            game,
            available,
            mixin_impls.get(name),
            report,
            !top,
            &inner,
        ),
        None => {
            report.missing.push(name.to_owned());
            report.classes += 1;
            if top {
                minimal(name)
            } else {
                let simple = name.rsplit('$').next().unwrap_or(name);
                format!("public static class {simple} {{ public {simple}() {{}} }}")
            }
        }
    }
}

/// Interfaces that a mod passes a lambda to. `render` otherwise gives every
/// interface method a `default` body, which leaves `LambdaMetafactory` without
/// an abstract method to bind. `SpawnPlacements.register` takes one such
/// predicate, so it is the one the bounded entity façade keeps abstract.
fn functional_interface(name: &str) -> bool {
    matches!(
        name,
        // `SpawnPlacements.register` takes this predicate.
        "net/minecraft/world/entity/SpawnPlacements$SpawnPredicate"
        // A renderer registry hands a constructor reference to one of these,
        // so the provider stays a single abstract method in the façade.
        | "net/minecraft/client/renderer/entity/EntityRendererProvider"
        | "net/minecraft/client/renderer/blockentity/BlockEntityRendererProvider"
    )
}

/// Bounded value classes that a generated body constructs. They have to travel
/// with the class that names them, or `javac` stops on a symbol the façade
/// never emitted. The closure in `generate_many` re-queues each entry, so a
/// support class can pull in its own support.
fn support_types(name: &str) -> &'static [&'static str] {
    match name {
        // `Player`/`ServerPlayer` carry an inventory and its abilities even when
        // the mod only names one player accessor; the bounded fields construct
        // both, so they travel with the player.
        "net/minecraft/world/entity/player/Player" | "net/minecraft/server/level/ServerPlayer" => {
            &[
                "net/minecraft/world/entity/player/Inventory",
                "net/minecraft/world/entity/player/Abilities",
            ]
        }
        // The bounded inventory names the stacks it stores.
        "net/minecraft/world/entity/player/Inventory" => &["net/minecraft/world/item/ItemStack"],
        // The bounded `Mob` value names these in its fields and its health,
        // attribute and navigation accessors. They travel with it so a mod
        // that only names one flag still gets a source that compiles.
        "net/minecraft/world/entity/Mob" => &[
            "net/minecraft/world/entity/SpawnGroupData",
            "net/minecraft/world/entity/ai/attributes/AttributeMap",
            "net/minecraft/world/entity/ai/navigation/PathNavigation",
            "net/minecraft/world/entity/ai/goal/GoalSelector",
        ],
        // The bounded `LivingEntity` carries health, hands and effects; a mod
        // that only names `getHealth` still needs those value types present,
        // plus the attribute instance and map its own fields construct.
        "net/minecraft/world/entity/LivingEntity" => &[
            "net/minecraft/world/item/ItemStack",
            "net/minecraft/world/effect/MobEffect",
            "net/minecraft/world/effect/MobEffectInstance",
            "net/minecraft/world/entity/ai/attributes/AttributeInstance",
            "net/minecraft/world/entity/ai/attributes/AttributeMap",
        ],
        // An `ItemStack` value names the item it holds and the component map it
        // carries; both must exist wherever the stack is generated.
        "net/minecraft/world/item/ItemStack" => &[
            "net/minecraft/world/item/Item",
            "net/minecraft/core/component/DataComponentType",
        ],
        // `TagKey` is a 26.3 record: its components are private, so the
        // generate closure does not see their types through the field list.
        "net/minecraft/tags/TagKey" => &[
            "net/minecraft/resources/ResourceKey",
            "net/minecraft/resources/Identifier",
        ],
        // The payload type is a one-component record (`Identifier id`) for the
        // same reason.
        "net/minecraft/network/protocol/common/custom/CustomPacketPayload$Type" => {
            &["net/minecraft/resources/Identifier"]
        }
        // A bounded `Entity` always carries its level, type and box, even when
        // the mod only names one position accessor. An entity-only target
        // otherwise generates a source that names types it never emitted.
        "net/minecraft/world/entity/Entity" => &[
            "net/minecraft/world/level/Level",
            "net/minecraft/world/entity/EntityType",
            "net/minecraft/world/phys/AABB",
        ],
        // A `BlockEntity` names its position, level, state and tag (and, in the
        // constructor the game declares, its type), so a block-entity-only
        // target gets the same bounded slice.
        "net/minecraft/world/level/block/entity/BlockEntity" => &[
            "net/minecraft/core/BlockPos",
            "net/minecraft/world/level/Level",
            "net/minecraft/world/level/block/state/BlockState",
            "net/minecraft/nbt/CompoundTag",
            "net/minecraft/world/level/block/entity/BlockEntityType",
        ],
        // The level's bounded value names the blocks and block entities it
        // stores, its dimension key and the positions it keys them by.
        "net/minecraft/world/level/Level" | "net/minecraft/server/level/ServerLevel" => &[
            "net/minecraft/core/BlockPos",
            "net/minecraft/world/level/block/state/BlockState",
            "net/minecraft/world/level/block/entity/BlockEntity",
            "net/minecraft/resources/ResourceKey",
        ],
        // A block state names its block and the fluid state its accessor
        // returns; both travel with the state.
        "net/minecraft/world/level/block/state/BlockState" => &[
            "net/minecraft/world/level/block/Block",
            "net/minecraft/world/level/material/FluidState",
        ],
        // `ResourceKey`'s bounded value names the identifier it was keyed by.
        "net/minecraft/resources/ResourceKey" => &["net/minecraft/resources/Identifier"],
        "net/minecraft/world/entity/EntityType"
        | "net/minecraft/world/entity/EntityType$Builder" => &[
            "net/minecraft/world/entity/EntityDimensions",
            "net/minecraft/world/entity/MobCategory",
        ],
        "net/minecraft/world/entity/EntityDimensions" => &[
            "net/minecraft/world/phys/AABB",
            "net/minecraft/world/phys/Vec3",
        ],
        "net/minecraft/world/entity/SpawnPlacements" => &[
            "net/minecraft/world/entity/SpawnPlacementType",
            "net/minecraft/world/entity/SpawnPlacementTypes",
            "net/minecraft/world/level/levelgen/Heightmap$Types",
        ],
        // The codec's state names the two callbacks `of` takes and the buffer
        // its encoder writes into, none of which a mod that only passes a
        // lambda to `of` has to mention itself.
        "net/minecraft/network/codec/StreamCodec" => &[
            "net/minecraft/network/codec/StreamEncoder",
            "net/minecraft/network/codec/StreamDecoder",
            "net/minecraft/network/FriendlyByteBuf",
        ],
        _ => &[],
    }
}

fn render(
    class: &Class,
    used: Option<&BTreeSet<(String, String)>>,
    game: &BTreeMap<String, Class>,
    available: &BTreeSet<String>,
    mixin_impls: Option<&Vec<MixinImpl>>,
    report: &mut Report,
    nested: bool,
    inner: &[String],
) -> String {
    let (package, simple) = match class.name.rsplit_once('/') {
        Some((package, simple)) => (Some(package), simple),
        None => (None, class.name.as_str()),
    };
    // A nested class's source name is its last `$` segment.
    let simple = if nested {
        simple.rsplit('$').next().unwrap_or(simple)
    } else {
        simple
    };
    let is_interface = class.access & 0x0200 != 0;
    let is_annotation = class.access & 0x2000 != 0;
    let is_enum = class.access & 0x4000 != 0;
    let is_record = class.superclass == "java/lang/Record";
    let modifier = if nested { "public static" } else { "public" };
    let external_super = !is_record
        && !class.superclass.is_empty()
        && !game.contains_key(&class.superclass)
        && (available.contains(&class.superclass) || class.superclass.starts_with("java/"))
        && (simple.ends_with("Exception") || simple.ends_with("Error"));
    let abstract_kw = if external_super { "abstract " } else { "" };
    let mut out = String::new();
    if let Some(package) = package
        && !nested
    {
        let _ = writeln!(out, "package {};", package.replace('/', "."));
    }
    // `EntityDimensions` is a small immutable value in 26.3 (the parity oracles
    // call its `scalable`/`fixed`/`makeBoundingBox` factories and its `width`
    // component). The façade needs the value, not the component plumbing: a
    // width/height/eye-height triple that builds a box the same way `Entity`
    // does, so `EntityType.getDimensions()` can hand mod code a usable object
    // without any world state. It is written by hand because a record cannot
    // carry the extra accessors and backing fields that snapshotted mod code
    // names.
    if class.name == "net/minecraft/world/entity/EntityDimensions" {
        out.push_str(concat!(
            "public class EntityDimensions {\n",
            "    protected float width;\n",
            "    protected float height;\n",
            "    protected float eyeHeight;\n",
            "    protected boolean fixed;\n",
            "    public EntityDimensions() { this(0.6f, 1.8f, 1.62f, false); }\n",
            "    public EntityDimensions(float width, float height) { this(width, height, height * 0.85f, false); }\n",
            "    public EntityDimensions(float width, float height, float eyeHeight) { this(width, height, eyeHeight, false); }\n",
            "    public EntityDimensions(float width, float height, float eyeHeight, boolean fixed) { this.width = width; this.height = height; this.eyeHeight = eyeHeight; this.fixed = fixed; }\n",
            "    public static net.minecraft.world.entity.EntityDimensions scalable(float width, float height) { return new net.minecraft.world.entity.EntityDimensions(width, height, height * 0.85f, false); }\n",
            "    public static net.minecraft.world.entity.EntityDimensions fixed(float width, float height) { return new net.minecraft.world.entity.EntityDimensions(width, height, height * 0.85f, true); }\n",
            "    public net.minecraft.world.entity.EntityDimensions scale(float factor) { return scale(factor, factor); }\n",
            "    public net.minecraft.world.entity.EntityDimensions scale(float widthFactor, float heightFactor) { return new net.minecraft.world.entity.EntityDimensions(width * widthFactor, height * heightFactor, eyeHeight * heightFactor, fixed); }\n",
            "    public net.minecraft.world.entity.EntityDimensions withEyeHeight(float eyeHeight) { return new net.minecraft.world.entity.EntityDimensions(width, height, eyeHeight, fixed); }\n",
            "    public net.minecraft.world.phys.AABB makeBoundingBox(double x, double y, double z) { double half = (double) (width / 2.0f); return new net.minecraft.world.phys.AABB(x - half, y, z - half, x + half, y + (double) height, z + half); }\n",
            "    public net.minecraft.world.phys.AABB makeBoundingBox(net.minecraft.world.phys.Vec3 pos) { /* bounded: Vec3 components are not read in the façade, so this is the zero-sized box at the origin */ return new net.minecraft.world.phys.AABB(0.0, 0.0, 0.0, 0.0, 0.0, 0.0); }\n",
            "    public float width() { return width; }\n",
            "    public float height() { return height; }\n",
            "    public float eyeHeight() { return eyeHeight; }\n",
            "    public boolean fixed() { return fixed; }\n",
            "    public float getWidth() { return width; }\n",
            "    public float getHeight() { return height; }\n",
            "    public float getEyeHeight() { return eyeHeight; }\n",
            "}\n",
        ));
        report.classes += 1;
        report.fields += 4;
        // Four constructors and fourteen value methods.
        report.methods += 18;
        return out;
    }
    if is_annotation {
        let _ = writeln!(out, "{modifier} @interface {simple} {{}}");
        return out;
    }
    let extends = if !is_record && !class.superclass.is_empty()
        && class.superclass != "java/lang/Object"
        // A record's source form is `record`, not `extends Record`.
        && class.superclass != "java/lang/Record"
        && class.superclass != "java/lang/Enum"
        // A real library superclass would owe its abstract methods.
        && (game.contains_key(&class.superclass)
            || ((available.contains(&class.superclass)
                || class.superclass.starts_with("java/"))
                && (simple.ends_with("Exception") || simple.ends_with("Error"))))
    {
        format!(" extends {}", java_name(&class.superclass))
    } else {
        String::new()
    };
    // Only interfaces keep their parents. A stub class that implemented the
    // game's interfaces would owe their abstract methods and would collide on
    // their default ones; the probe wants the class, not the cast.
    let interfaces: Vec<String> = if is_interface {
        class
            .interfaces
            .iter()
            .filter(|i| i.as_str() != class.name)
            .map(|i| java_name(i))
            .collect()
    } else {
        Vec::new()
    };
    let implements = if interfaces.is_empty() {
        String::new()
    } else {
        format!(" extends {}", interfaces.join(", "))
    };
    let mixin_interfaces = if !is_interface {
        let names = mixin_impls
            .into_iter()
            .flatten()
            .filter(|mixin| available.contains(&mixin.interface))
            .map(|mixin| java_name(&mixin.interface))
            .collect::<BTreeSet<_>>();
        if names.is_empty() {
            String::new()
        } else {
            format!(
                " implements {}",
                names.into_iter().collect::<Vec<_>>().join(", ")
            )
        }
    } else {
        String::new()
    };
    if is_interface {
        let _ = writeln!(out, "{modifier} interface {simple}{implements} {{");
    } else if is_enum {
        let constants: Vec<&str> = class
            .declared
            .iter()
            .filter(|d| d.is_enum_constant())
            .map(|d| d.name.as_str())
            .collect();
        let _ = writeln!(out, "{modifier} enum {simple} {{ {};", constants.join(", "));
    } else if is_record {
        let components = class
            .declared
            .iter()
            .filter(|d| d.kind == MemberKind::Field && !d.is_static() && d.access & 0x1000 == 0)
            .map(|field| format!("{} {}", java_type(&field.descriptor), field.name))
            .collect::<Vec<_>>()
            .join(", ");
        let implements = class
            .interfaces
            .iter()
            .map(|i| java_name(i))
            .collect::<Vec<_>>();
        let implements = if implements.is_empty() {
            String::new()
        } else {
            format!(" implements {}", implements.join(", "))
        };
        let _ = writeln!(
            out,
            "{modifier} {abstract_kw}record {simple}({components}){implements} {{"
        );
    } else {
        let codec_impl =
            if class.name == "net/minecraft/util/StringRepresentable$StringRepresentableCodec" {
                " implements com.mojang.serialization.Codec"
            } else {
                ""
            };
        let trigger = if class.name == "net/minecraft/advancements/triggers/SimpleCriterionTrigger"
        {
            (
                "abstract ",
                " implements net.minecraft.advancements.triggers.CriterionTrigger",
            )
        } else if class.name == "net/minecraft/network/chat/MutableComponent" {
            ("", " implements net.minecraft.network.chat.Component")
        } else {
            ("", "")
        };
        let builder_contract = if class.name == "net/minecraft/world/entity/EntityType$Builder"
            && available
                .contains("net/fabricmc/fabric/impl/object/builder/FabricEntityTypeImpl$Builder")
        {
            " implements net.fabricmc.fabric.impl.object.builder.FabricEntityTypeImpl.Builder"
        } else {
            ""
        };
        let builder_contract = format!("{builder_contract}{mixin_interfaces}");
        let _ = writeln!(
            out,
            "{modifier} {abstract_kw}{trigger_abstract}class {simple}{extends}{codec_impl}{trigger_contract}{builder_contract} {{",
            abstract_kw = abstract_kw,
            trigger_abstract = if abstract_kw.is_empty() {
                trigger.0
            } else {
                ""
            },
            trigger_contract = trigger.1,
            builder_contract = builder_contract
        );
    }
    let identifier = class.name == "net/minecraft/resources/Identifier";
    let resource_key = class.name == "net/minecraft/resources/ResourceKey";
    if identifier {
        let _ = writeln!(
            out,
            "    private String horizonNamespace = \"minecraft\";\n    private String horizonPath = \"\";"
        );
        let _ = writeln!(
            out,
            "    public Identifier() {{}}\n    public Identifier(String namespace, String path) {{ this.horizonNamespace = namespace; this.horizonPath = path; }}"
        );
    }
    if resource_key {
        let _ = writeln!(
            out,
            "    private net.minecraft.resources.Identifier horizonRegistryName = new net.minecraft.resources.Identifier();\n    private net.minecraft.resources.Identifier horizonIdentifier = new net.minecraft.resources.Identifier();\n    public ResourceKey() {{}}\n    public ResourceKey(net.minecraft.resources.Identifier registryName, net.minecraft.resources.Identifier identifier) {{ this.horizonRegistryName = registryName; this.horizonIdentifier = identifier; }}"
        );
    }
    if matches!(
        class.name.as_str(),
        "net/minecraft/core/Registry" | "net/minecraft/core/DefaultedRegistry"
    ) {
        // An interface's bounded store is a shared constant (`protected` is
        // not a legal interface modifier); a class keeps the per-instance map.
        let prefix = if is_interface { "" } else { "protected final " };
        let _ = writeln!(
            out,
            "    {prefix}java.util.Map<net.minecraft.resources.Identifier, Object> horizonEntries = new java.util.LinkedHashMap<>();\n    {prefix}java.util.Map<net.minecraft.resources.Identifier, net.minecraft.resources.ResourceKey> horizonKeys = new java.util.LinkedHashMap<>();"
        );
    }
    let holder_impl = class.name.starts_with("net/minecraft/core/Holder$") && !is_interface;
    if holder_impl {
        let _ = writeln!(
            out,
            "    protected Object horizonValue;\n    protected net.minecraft.resources.ResourceKey horizonKey;\n    protected net.minecraft.resources.Identifier horizonId;\n    public void horizonBind(Object value, net.minecraft.resources.ResourceKey key, net.minecraft.resources.Identifier id) {{ this.horizonValue = value; this.horizonKey = key; this.horizonId = id; }}"
        );
    }
    if class.name == "net/neoforged/neoforge/registries/DeferredHolder" {
        let _ = writeln!(
            out,
            "    protected Object horizonValue;\n    protected java.util.function.Supplier horizonSupplier;\n    protected net.minecraft.resources.Identifier horizonId = new net.minecraft.resources.Identifier();\n    protected net.minecraft.resources.ResourceKey horizonKey = new net.minecraft.resources.ResourceKey();\n    public DeferredHolder() {{}}\n    public DeferredHolder(net.minecraft.resources.Identifier id, net.minecraft.resources.ResourceKey key, java.util.function.Supplier supplier) {{ this.horizonId = id; this.horizonKey = key; this.horizonSupplier = supplier; }}\n    public void horizonBind(net.minecraft.resources.Identifier id, net.minecraft.resources.ResourceKey key, java.util.function.Supplier supplier) {{ this.horizonId = id; this.horizonKey = key; this.horizonSupplier = supplier; }}"
        );
    }
    if matches!(
        class.name.as_str(),
        "net/minecraft/core/Vec3i" | "net/minecraft/core/BlockPos"
    ) {
        let _ = writeln!(
            out,
            "    protected int horizonX;\n    protected int horizonY;\n    protected int horizonZ;"
        );
    }
    if class.name == "net/minecraft/world/phys/Vec3" {
        let _ = writeln!(
            out,
            "    protected double horizonX;\n    protected double horizonY;\n    protected double horizonZ;"
        );
    }
    if class.name == "net/minecraft/network/chat/MutableComponent" {
        let _ = writeln!(
            out,
            "    protected String horizonText = \"\";\n    protected String horizonKey = \"\";"
        );
    }
    if class.name == "net/minecraft/network/codec/StreamCodec" && !is_interface {
        let _ = write!(out, "{STREAM_CODEC_STATE}");
        // The game's codec inherits these two from `StreamEncoder` and
        // `StreamDecoder`; the façade's stub does not model those parents, so
        // the pair is written here with the bodies below.
        let _ = writeln!(
            out,
            "    public void encode(net.minecraft.network.FriendlyByteBuf p0, Object p1) {{ {ENCODE_CODEC} }}"
        );
        let _ = writeln!(
            out,
            "    public Object decode(net.minecraft.network.FriendlyByteBuf p0) {{ {DECODE_CODEC} }}"
        );
        // The factories that take callbacks: declared generically over the
        // same type parameters as the game's, so a mod's lambda (and a
        // `ByteBufCodecs` factory's) infers its parameter types from the
        // buffer it is handed rather than from `Object`.
        let _ = writeln!(
            out,
            "    public static <B, V> net.minecraft.network.codec.StreamCodec of(net.minecraft.network.codec.StreamEncoder<B, V> p0, net.minecraft.network.codec.StreamDecoder<B, V> p1) {{ net.minecraft.network.codec.StreamCodec codec = new net.minecraft.network.codec.StreamCodec(); return codec.horizonOf(p0, p1); }}"
        );
        let _ = writeln!(
            out,
            "    public static <B, V> net.minecraft.network.codec.StreamCodec ofMember(net.minecraft.network.codec.StreamMemberEncoder<B, V> p0, net.minecraft.network.codec.StreamDecoder<B, V> p1) {{ net.minecraft.network.codec.StreamCodec codec = new net.minecraft.network.codec.StreamCodec(); return codec.horizonOf(null, p1); }}"
        );
    }
    if class.name == "net/minecraft/world/item/Item$Properties" {
        let _ = writeln!(
            out,
            "    protected int horizonMaxStackSize = 64;\n    protected int horizonMaxDamage;\n    protected net.minecraft.world.item.Item horizonCraftRemainder;\n    protected boolean horizonFireResistant;"
        );
    }
    if class.name == "net/minecraft/world/item/Item" {
        let _ = writeln!(
            out,
            "    protected int horizonMaxStackSize = 64;\n    protected int horizonMaxDamage;\n    protected net.minecraft.world.item.Item horizonCraftRemainder;\n    protected boolean horizonFireResistant;"
        );
    }
    if class.name == "net/minecraft/world/level/block/state/BlockBehaviour$Properties" {
        let _ = writeln!(
            out,
            "    protected float horizonDestroyTime;\n    protected float horizonExplosionResistance;\n    protected float horizonFriction = 0.6f;\n    protected float horizonSpeedFactor = 1.0f;\n    protected float horizonJumpFactor = 1.0f;\n    protected boolean horizonNoOcclusion;\n    protected boolean horizonNoLootTable;\n    protected boolean horizonCorrectTool;\n    protected boolean horizonRandomTicks;"
        );
    }
    if class.name == "net/minecraft/world/phys/AABB" {
        let _ = writeln!(
            out,
            "    protected double horizonMinX;\n    protected double horizonMinY;\n    protected double horizonMinZ;\n    protected double horizonMaxX;\n    protected double horizonMaxY;\n    protected double horizonMaxZ;"
        );
    }
    if class.name == "net/minecraft/world/phys/HitResult" {
        let _ = writeln!(
            out,
            "    protected net.minecraft.world.phys.Vec3 horizonLocation = new net.minecraft.world.phys.Vec3();\n    protected net.minecraft.core.Direction horizonDirection = net.minecraft.core.Direction.UP;\n    protected boolean horizonInside;"
        );
    }
    if class.name == "net/minecraft/world/phys/BlockHitResult" {
        let _ = writeln!(
            out,
            "    protected net.minecraft.core.BlockPos horizonBlockPos = new net.minecraft.core.BlockPos();"
        );
    }
    if class.name == "net/minecraft/world/item/ItemStack" {
        let _ = writeln!(
            out,
            "    protected net.minecraft.world.item.Item horizonItem = new net.minecraft.world.item.Item();\n    protected int horizonCount;\n    protected int horizonDamage;\n    protected java.util.Map<net.minecraft.core.component.DataComponentType, Object> horizonComponents = new java.util.HashMap<>();"
        );
    }
    if class.name == "net/minecraft/network/FriendlyByteBuf" {
        let _ = write!(out, "{FRIENDLY_BYTE_BUF_STATE}");
    }
    if class.name == "net/minecraft/world/level/block/state/BlockState" {
        let _ = writeln!(
            out,
            "    protected net.minecraft.world.level.block.Block horizonBlock = new net.minecraft.world.level.block.Block();\n    protected java.util.IdentityHashMap<Object, Object> horizonValues = new java.util.IdentityHashMap<>();"
        );
    }
    if class.name == "net/minecraft/world/level/material/FluidState" {
        let _ = writeln!(
            out,
            "    protected boolean horizonEmpty = true;\n    protected int horizonAmount;"
        );
    }
    if class.name == "net/minecraft/nbt/CompoundTag" {
        let _ = writeln!(
            out,
            "    protected java.util.Map<String, Object> horizonValues = new java.util.LinkedHashMap<>();"
        );
    }
    if class.name == "net/minecraft/nbt/ListTag" {
        let _ = writeln!(
            out,
            "    protected java.util.List<net.minecraft.nbt.Tag> horizonValues = new java.util.ArrayList<>();"
        );
    }
    if matches!(
        class.name.as_str(),
        "net/minecraft/world/level/Level" | "net/minecraft/server/level/ServerLevel"
    ) {
        let _ = writeln!(
            out,
            "    protected long horizonGameTime;\n    protected float horizonRainLevel;\n    protected float horizonThunderLevel;\n    protected net.minecraft.resources.ResourceKey horizonDimension = new net.minecraft.resources.ResourceKey();\n    protected java.util.Map<String, net.minecraft.world.level.block.state.BlockState> horizonBlocks = new java.util.LinkedHashMap<>();\n    protected java.util.Map<String, net.minecraft.world.level.block.entity.BlockEntity> horizonBlockEntities = new java.util.LinkedHashMap<>();\n    protected String horizonBlockKey(net.minecraft.core.BlockPos pos) {{ return pos.getX() + \",\" + pos.getY() + \",\" + pos.getZ(); }}"
        );
    }
    if class.name == "net/minecraft/world/level/block/entity/BlockEntity" {
        let _ = writeln!(
            out,
            "    protected net.minecraft.core.BlockPos horizonBlockPos = new net.minecraft.core.BlockPos();\n    protected net.minecraft.world.level.Level horizonLevel = new net.minecraft.world.level.Level();\n    protected net.minecraft.world.level.block.state.BlockState horizonBlockState = new net.minecraft.world.level.block.state.BlockState();\n    protected net.minecraft.nbt.CompoundTag horizonData = new net.minecraft.nbt.CompoundTag();\n    protected boolean horizonRemoved;"
        );
    }
    if class.name == "net/minecraft/world/entity/Entity" {
        let _ = writeln!(
            out,
            "    protected double horizonX;\n    protected double horizonY;\n    protected double horizonZ;\n    protected double horizonDeltaX;\n    protected double horizonDeltaY;\n    protected double horizonDeltaZ;\n    protected float horizonXRot;\n    protected float horizonYRot;\n    protected float horizonBbWidth = 0.6f;\n    protected float horizonBbHeight = 1.8f;\n    protected float horizonEyeHeight = 1.62f;\n    protected net.minecraft.world.level.Level horizonLevel = new net.minecraft.world.level.Level();\n    protected net.minecraft.world.entity.EntityType horizonType = new net.minecraft.world.entity.EntityType();\n    protected java.util.UUID horizonUuid = new java.util.UUID(0L, 0L);\n    protected int horizonEntityId;\n    protected net.minecraft.world.phys.AABB horizonBoundingBox = new net.minecraft.world.phys.AABB(0.0, 0.0, 0.0, 0.0, 0.0, 0.0);\n    protected boolean horizonOnGround;\n    protected boolean horizonRemoved;\n    protected boolean horizonOnFire;\n    protected boolean horizonInWater;\n    protected boolean horizonInLava;\n    protected boolean horizonCrouching;\n    protected boolean horizonSprinting;"
        );
    }
    if class.name == "net/minecraft/world/entity/EntityType$Builder" {
        let _ = writeln!(
            out,
            "    protected float horizonWidth = 0.6f;\n    protected float horizonHeight = 1.8f;\n    protected int horizonTrackingRange = 5;\n    protected int horizonUpdateInterval = 3;\n    protected boolean horizonFireImmune;\n    protected boolean horizonCanSpawnFarFromPlayer;\n    protected boolean horizonCanSummon = true;\n    protected boolean horizonSaves = true;\n    protected net.minecraft.world.entity.MobCategory horizonCategory = net.minecraft.world.entity.MobCategory.MISC;"
        );
    }
    if class.name == "net/minecraft/world/entity/EntityType" {
        let _ = writeln!(
            out,
            "    protected float horizonWidth = 0.6f;\n    protected float horizonHeight = 1.8f;\n    protected int horizonTrackingRange = 5;\n    protected int horizonUpdateInterval = 3;\n    protected boolean horizonFireImmune;\n    protected boolean horizonCanSpawnFarFromPlayer;\n    protected boolean horizonCanSummon = true;\n    protected boolean horizonSaves = true;\n    protected String horizonDescriptionId = \"entity.minecraft.unknown\";\n    protected net.minecraft.world.entity.MobCategory horizonCategory = net.minecraft.world.entity.MobCategory.MISC;\n    public EntityType(float width, float height, int trackingRange, int updateInterval, boolean fireImmune) {{ this(width, height, trackingRange, updateInterval, fireImmune, net.minecraft.world.entity.MobCategory.MISC); }}\n    public EntityType(float width, float height, int trackingRange, int updateInterval, boolean fireImmune, net.minecraft.world.entity.MobCategory category) {{ this.horizonWidth = width; this.horizonHeight = height; this.horizonTrackingRange = trackingRange; this.horizonUpdateInterval = updateInterval; this.horizonFireImmune = fireImmune; this.horizonCategory = category == null ? net.minecraft.world.entity.MobCategory.MISC : category; }}"
        );
    }
    if class.name == "net/minecraft/world/entity/SpawnPlacements" {
        // Registration is real state, so a mod's `register`/`getPlacementType`
        // pair is self-consistent. Keys are the `EntityType` values the mod
        // registers (identity, like the game's own table). The 26.3 names come
        // from Horizon's own spawn model and `VanillaSpawnRulesOracle`
        // (`SpawnPlacementTypes`, `getPlacementType`, `getHeightmapType`,
        // `Heightmap.Types`); no decompiled 26.3 sources were present here.
        let _ = writeln!(
            out,
            "    protected static final java.util.Map<Object, net.minecraft.world.entity.SpawnPlacementType> horizonPlacements = new java.util.LinkedHashMap<>();\n    protected static final java.util.Map<Object, net.minecraft.world.level.levelgen.Heightmap.Types> horizonHeightmaps = new java.util.LinkedHashMap<>();\n    protected static final java.util.Map<Object, Boolean> horizonSpawnRules = new java.util.LinkedHashMap<>();"
        );
    }
    if class.name == "net/minecraft/world/entity/Mob" {
        let _ = writeln!(
            out,
            "    protected boolean horizonNoAi;\n    protected boolean horizonAggressive;\n    protected boolean horizonBaby;\n    protected boolean horizonPersistenceRequired;\n    protected net.minecraft.world.entity.LivingEntity horizonTarget;\n    protected java.util.Map<Object, net.minecraft.world.entity.ai.attributes.AttributeInstance> horizonAttributes = new java.util.IdentityHashMap<>();\n    protected net.minecraft.world.entity.ai.attributes.AttributeMap horizonAttributeMap = new net.minecraft.world.entity.ai.attributes.AttributeMap();\n    protected net.minecraft.world.entity.ai.navigation.PathNavigation horizonNavigation = new net.minecraft.world.entity.ai.navigation.PathNavigation();\n    protected net.minecraft.world.entity.ai.goal.GoalSelector horizonGoalSelector = new net.minecraft.world.entity.ai.goal.GoalSelector();\n    protected net.minecraft.world.entity.ai.goal.GoalSelector horizonTargetSelector = new net.minecraft.world.entity.ai.goal.GoalSelector();\n    protected net.minecraft.world.entity.SpawnGroupData horizonSpawnGroupData;"
        );
    }
    if class.name == "net/minecraft/world/entity/ai/attributes/Attribute" {
        let _ = writeln!(
            out,
            "    protected double horizonBaseValue;\n    protected double horizonMinValue = Double.NEGATIVE_INFINITY;\n    protected double horizonMaxValue = Double.POSITIVE_INFINITY;\n    protected String horizonDescriptionId = \"generic\";\n    protected boolean horizonSyncable;\n    protected Object horizonSentiment;"
        );
    }
    if class.name == "net/minecraft/world/entity/ai/attributes/RangedAttribute" {
        let _ = writeln!(
            out,
            "    protected double horizonBaseValue;\n    protected double horizonMinValue;\n    protected double horizonMaxValue;"
        );
    }
    if class.name == "net/minecraft/world/entity/ai/attributes/AttributeSupplier" {
        let _ = writeln!(
            out,
            "    protected java.util.Map<Object, net.minecraft.world.entity.ai.attributes.AttributeInstance> horizonAttributes = new java.util.IdentityHashMap<>();"
        );
    }
    if class.name == "net/minecraft/world/entity/ai/attributes/AttributeInstance" {
        let _ = writeln!(
            out,
            "    protected double horizonBaseValue;\n    protected double horizonValue;"
        );
    }
    if class.name == "net/minecraft/network/syncher/SynchedEntityData" {
        let _ = writeln!(
            out,
            "    protected static int horizonNextId;\n    protected java.util.Map<net.minecraft.network.syncher.EntityDataAccessor, Object> horizonItems = new java.util.HashMap<>();\n    protected java.util.Set<net.minecraft.network.syncher.EntityDataAccessor> horizonDirty = new java.util.HashSet<>();\n    protected java.util.List<Object> horizonAssigned = new java.util.ArrayList<>();"
        );
    }
    // `EntityDataAccessor` and `DamageType` are records in 26.3: javac rejects
    // instance fields on a record, so their state is the record components
    // themselves. Their generated accessors already return the real values.
    if class.name == "net/minecraft/world/damagesource/DamageSource" {
        let _ = writeln!(
            out,
            "    protected net.minecraft.world.damagesource.DamageType horizonType;\n    protected net.minecraft.world.entity.Entity horizonEntity;\n    protected net.minecraft.world.entity.Entity horizonDirectEntity;\n    protected net.minecraft.world.phys.Vec3 horizonSourcePosition;"
        );
    }
    if class.name == "net/minecraft/world/entity/LivingEntity" {
        let _ = writeln!(
            out,
            "    protected float horizonHealth = 20.0f;\n    protected float horizonMaxHealth = 20.0f;\n    protected net.minecraft.world.item.ItemStack horizonMainHand = new net.minecraft.world.item.ItemStack();\n    protected net.minecraft.world.item.ItemStack horizonOffhand = new net.minecraft.world.item.ItemStack();\n    protected int horizonArmorValue;\n    protected java.util.Map<net.minecraft.world.effect.MobEffect, net.minecraft.world.effect.MobEffectInstance> horizonEffects = new java.util.IdentityHashMap<>();\n    protected java.util.Map<Object, net.minecraft.world.entity.ai.attributes.AttributeInstance> horizonAttributes = new java.util.IdentityHashMap<>();\n    protected net.minecraft.world.entity.ai.attributes.AttributeMap horizonAttributeMap = new net.minecraft.world.entity.ai.attributes.AttributeMap();"
        );
    }
    if class.name == "net/minecraft/world/effect/MobEffectInstance" {
        let _ = writeln!(
            out,
            "    protected net.minecraft.world.effect.MobEffect horizonEffect = new net.minecraft.world.effect.MobEffect();\n    protected int horizonDuration;\n    protected int horizonAmplifier;\n    protected boolean horizonAmbient;\n    protected boolean horizonVisible = true;\n    protected boolean horizonShowIcon = true;"
        );
    }
    if matches!(
        class.name.as_str(),
        "net/minecraft/world/entity/player/Player" | "net/minecraft/server/level/ServerPlayer"
    ) {
        let _ = writeln!(
            out,
            "    protected boolean horizonCreative;\n    protected boolean horizonSpectator;\n    protected String horizonScoreboardName = \"Player\";\n    protected int horizonExperienceLevel;\n    protected float horizonExperienceProgress;\n    protected net.minecraft.world.entity.player.Inventory horizonInventory = new net.minecraft.world.entity.player.Inventory();\n    protected net.minecraft.world.entity.player.Abilities horizonAbilities = new net.minecraft.world.entity.player.Abilities();"
        );
    }
    if class.name == "net/minecraft/world/entity/player/Inventory" {
        out.push_str("    protected net.minecraft.world.item.ItemStack[] horizonItems = new net.minecraft.world.item.ItemStack[36];\n    protected int horizonSelected;\n");
    }
    if class.name == "net/minecraft/world/entity/player/Abilities" {
        out.push_str("    protected float horizonFlyingSpeed = 0.05f;\n    protected float horizonWalkingSpeed = 0.1f;\n");
    }
    if class.name == "net/minecraft/client/KeyMapping" {
        let _ = writeln!(out, "    private String horizonName = \"\";");
    }
    if class.name == "net/minecraft/world/level/block/Block" {
        let _ = writeln!(
            out,
            "    public void registerDefaultState(net.minecraft.world.level.block.state.BlockState state) {{}}"
        );
    }
    if class.name == "net/minecraft/util/StringRepresentable$StringRepresentableCodec" {
        let _ = writeln!(
            out,
            "    @SuppressWarnings({{\"rawtypes\", \"unchecked\"}}) public com.mojang.serialization.DataResult decode(com.mojang.serialization.DynamicOps ops, Object input) {{ return (com.mojang.serialization.DataResult) com.mojang.serialization.Codec.STRING.decode(ops, input); }}"
        );
        let _ = writeln!(
            out,
            "    @SuppressWarnings({{\"rawtypes\", \"unchecked\"}}) public com.mojang.serialization.DataResult encode(Object value, com.mojang.serialization.DynamicOps ops, Object prefix) {{ return (com.mojang.serialization.DataResult) com.mojang.serialization.Codec.STRING.encode(String.valueOf(value), ops, prefix); }}"
        );
    }
    let mut methods = 0;
    let mut fields = 0;
    for field in class.declared.iter().filter(|d| {
        d.kind == MemberKind::Field
            && !d.is_enum_constant()
            && (!is_record || d.is_static())
            && (!is_enum || field_is_enum_abi(d))
    }) {
        if !(field.is_public() || field.is_protected()) {
            continue;
        }
        let name = &field.name;
        let ty = java_type(&field.descriptor);
        let default = runtime_field_default_named(
            &class.name,
            name,
            &field.descriptor,
            default_value(&field.descriptor),
        );
        if is_interface {
            let _ = writeln!(out, "    public static final {ty} {name} = {default};");
        } else if field.is_static() {
            let initializer = if default != "null" {
                format!(" = {default}")
            } else {
                String::new()
            };
            let _ = writeln!(out, "    public static {ty} {name}{initializer};");
        } else {
            let _ = writeln!(out, "    public {ty} {name};");
        }
        fields += 1;
    }
    // Constructors, so a mod's `super(...)` links.
    let mut no_arg = identifier || resource_key;
    // The `Identifier`/`ResourceKey` bodies above write their constructors by
    // hand. A game jar that also declares the same signature (older
    // `ResourceLocation` releases have a public `(String, String)` one) must
    // not get a second, empty copy.
    let mut provided_ctors: BTreeSet<String> = BTreeSet::new();
    if identifier {
        provided_ctors.insert("()V".to_owned());
        provided_ctors.insert("(Ljava/lang/String;Ljava/lang/String;)V".to_owned());
    }
    if resource_key {
        provided_ctors.insert("()V".to_owned());
        provided_ctors.insert(
            "(Lnet/minecraft/resources/Identifier;Lnet/minecraft/resources/Identifier;)V"
                .to_owned(),
        );
    }
    for method in class
        .declared
        .iter()
        .filter(|d| d.kind == MemberKind::Method && d.name == "<init>")
    {
        if !(method.is_public() || method.is_protected()) || is_interface || is_record {
            continue;
        }
        no_arg |= method.descriptor == "()V";
        if !provided_ctors.insert(method.descriptor.clone()) {
            continue;
        }
        let params = parameters(&method.descriptor);
        let body = if matches!(
            (class.name.as_str(), method.descriptor.as_str()),
            (
                "net/minecraft/core/Vec3i" | "net/minecraft/core/BlockPos",
                "(III)V"
            )
        ) || (class.name == "net/minecraft/world/phys/Vec3"
            && method.descriptor == "(DDD)V")
        {
            "this.horizonX = p0; this.horizonY = p1; this.horizonZ = p2;"
        } else if class.name == "net/minecraft/network/syncher/EntityDataAccessor"
            && method.descriptor == "(I)V"
        {
            "this.horizonId = p0;"
        } else if class.name == "net/minecraft/world/entity/ai/attributes/Attribute"
            && method.descriptor == "(Ljava/lang/String;D)V"
        {
            "this.horizonDescriptionId = p0; this.horizonBaseValue = p1;"
        } else if class.name == "net/minecraft/world/entity/ai/attributes/RangedAttribute"
            && method.descriptor == "(Ljava/lang/String;DDD)V"
        {
            "this.horizonDescriptionId = p0; this.horizonBaseValue = p1; this.horizonMinValue = p2; this.horizonMaxValue = p3;"
        } else if class.name == "net/minecraft/world/phys/AABB" && method.descriptor == "(DDDDDD)V"
        {
            "this.horizonMinX = p0; this.horizonMinY = p1; this.horizonMinZ = p2; this.horizonMaxX = p3; this.horizonMaxY = p4; this.horizonMaxZ = p5; this.minX = p0; this.minY = p1; this.minZ = p2; this.maxX = p3; this.maxY = p4; this.maxZ = p5;"
        } else if class.name == "net/minecraft/world/phys/AABB"
            && method.descriptor == "(Lnet/minecraft/core/BlockPos;)V"
        {
            "this.horizonMinX = p0.getX(); this.horizonMinY = p0.getY(); this.horizonMinZ = p0.getZ(); this.horizonMaxX = p0.getX() + 1.0; this.horizonMaxY = p0.getY() + 1.0; this.horizonMaxZ = p0.getZ() + 1.0; this.minX = horizonMinX; this.minY = horizonMinY; this.minZ = horizonMinZ; this.maxX = horizonMaxX; this.maxY = horizonMaxY; this.maxZ = horizonMaxZ;"
        } else if class.name == "net/minecraft/world/phys/AABB"
            && method.descriptor
                == "(Lnet/minecraft/world/phys/Vec3;Lnet/minecraft/world/phys/Vec3;)V"
        {
            "this.horizonMinX = Math.min(p0.x(), p1.x()); this.horizonMinY = Math.min(p0.y(), p1.y()); this.horizonMinZ = Math.min(p0.z(), p1.z()); this.horizonMaxX = Math.max(p0.x(), p1.x()); this.horizonMaxY = Math.max(p0.y(), p1.y()); this.horizonMaxZ = Math.max(p0.z(), p1.z()); this.minX = horizonMinX; this.minY = horizonMinY; this.minZ = horizonMinZ; this.maxX = horizonMaxX; this.maxY = horizonMaxY; this.maxZ = horizonMaxZ;"
        } else if class.name == "net/minecraft/world/phys/BlockHitResult"
            && method.descriptor
                == "(Lnet/minecraft/world/phys/Vec3;Lnet/minecraft/core/Direction;Lnet/minecraft/core/BlockPos;Z)V"
        {
            "this.horizonLocation = p0; this.horizonDirection = p1; this.horizonBlockPos = p2; this.horizonInside = p3;"
        } else if class.name == "net/minecraft/world/level/block/Block" {
            "this.stateDefinition = new net.minecraft.world.level.block.state.StateDefinition();"
        } else if class.name == "net/minecraft/client/KeyMapping"
            && method.descriptor.starts_with("(Ljava/lang/String;")
        {
            "this.horizonName = p0;"
        } else if class.name == "net/minecraft/world/item/ItemStack" {
            match method.descriptor.as_str() {
                "(Lnet/minecraft/world/item/Item;I)V" => {
                    "this.horizonItem = p0; this.horizonCount = p1;"
                }
                "(Lnet/minecraft/world/item/Item;)V" => {
                    "this.horizonItem = p0; this.horizonCount = 1;"
                }
                _ => "",
            }
        } else if class.name == "net/minecraft/world/item/Item"
            && method.descriptor == "(Lnet/minecraft/world/item/Item$Properties;)V"
        {
            "if (p0 != null) { this.horizonMaxStackSize = p0.horizonMaxStackSize; this.horizonMaxDamage = p0.horizonMaxDamage; this.horizonCraftRemainder = p0.horizonCraftRemainder; this.horizonFireResistant = p0.horizonFireResistant; }"
        } else if class.name == "net/minecraft/world/effect/MobEffectInstance" {
            match method.descriptor.as_str() {
                "(Lnet/minecraft/world/effect/MobEffect;I)V" => {
                    "this.horizonEffect = p0; this.horizonDuration = p1;"
                }
                "(Lnet/minecraft/world/effect/MobEffect;IIZZZ)V" => {
                    "this.horizonEffect = p0; this.horizonDuration = p1; this.horizonAmplifier = p2; this.horizonAmbient = p3; this.horizonVisible = p4; this.horizonShowIcon = p5;"
                }
                _ => "",
            }
        } else if class.name
            == "net/minecraft/network/protocol/common/custom/CustomPacketPayload$Type"
            && method.descriptor == "(Lnet/minecraft/resources/Identifier;)V"
        {
            "this.horizonId = p0;"
        } else if class.name == "net/minecraft/network/FriendlyByteBuf"
            && method.descriptor == "(Lio/netty/buffer/ByteBuf;)V"
        {
            // A Netty-backed buffer copies its readable window into the
            // bounded façade buffer, so a payload the game hands in decodes
            // through the same cursor the façade wrote with.
            "if (p0 != null) { int length = Math.min(p0.readableBytes(), HORIZON_MAX_BYTES); if (length > 0) { byte[] bytes = new byte[length]; p0.readBytes(bytes); this.horizonBytes = bytes; this.horizonWriteIndex = length; } }"
        } else if class.name == "net/minecraft/world/level/block/entity/BlockEntity"
            && method.descriptor
                == "(Lnet/minecraft/world/level/block/entity/BlockEntityType;Lnet/minecraft/core/BlockPos;Lnet/minecraft/world/level/block/state/BlockState;)V"
        {
            "this.horizonBlockPos = p1; this.horizonBlockState = p2;"
        } else if class.name == "net/minecraft/world/level/block/entity/BlockEntity"
            && method.descriptor
                == "(Lnet/minecraft/core/BlockPos;Lnet/minecraft/world/level/block/state/BlockState;)V"
        {
            "this.horizonBlockPos = p0; this.horizonBlockState = p1;"
        } else {
            ""
        };
        let _ = writeln!(out, "    public {simple}({params}) {{ {body} }}");
    }
    // Every stub has a no-arg constructor, so a subclass's implicit `super()`
    // links even when the real class only has constructors with arguments.
    if !is_interface && !is_enum && !is_record && !no_arg {
        let body = if matches!(
            class.name.as_str(),
            "net/minecraft/core/Vec3i" | "net/minecraft/core/BlockPos"
        ) {
            "this.horizonX = 0; this.horizonY = 0; this.horizonZ = 0;"
        } else if class.name == "net/minecraft/world/phys/Vec3" {
            "this.horizonX = 0.0; this.horizonY = 0.0; this.horizonZ = 0.0;"
        } else if class.name == "net/minecraft/world/phys/AABB" {
            "this.horizonMinX = 0.0; this.horizonMinY = 0.0; this.horizonMinZ = 0.0; this.horizonMaxX = 0.0; this.horizonMaxY = 0.0; this.horizonMaxZ = 0.0; this.minX = 0.0; this.minY = 0.0; this.minZ = 0.0; this.maxX = 0.0; this.maxY = 0.0; this.maxZ = 0.0;"
        } else if class.name == "net/minecraft/world/level/block/Block" {
            "this.stateDefinition = new net.minecraft.world.level.block.state.StateDefinition();"
        } else if class.name == "net/minecraft/client/KeyMapping" {
            "this.horizonName = \"\";"
        } else if class.name == "net/minecraft/world/item/ItemStack" {
            "this.horizonItem = new net.minecraft.world.item.Item(); this.horizonCount = 0;"
        } else {
            ""
        };
        let _ = writeln!(out, "    public {simple}() {{ {body} }}");
    }
    if class.name == "net/minecraft/world/entity/EntityType$Builder"
        && available
            .contains("net/fabricmc/fabric/impl/object/builder/FabricEntityTypeImpl$Builder")
    {
        let _ = writeln!(
            out,
            "    public void fabric_setLivingEntityBuilder(net.fabricmc.fabric.impl.object.builder.FabricEntityTypeImpl.Builder.Living p0) {{}}"
        );
        let _ = writeln!(
            out,
            "    public void fabric_setMobEntityBuilder(net.fabricmc.fabric.impl.object.builder.FabricEntityTypeImpl.Builder.Mob p0) {{}}"
        );
    }
    if is_record {
        let defaults = class
            .declared
            .iter()
            .filter(|d| d.kind == MemberKind::Field && !d.is_static() && d.access & 0x1000 == 0)
            .map(|field| constructor_default_value(&field.descriptor))
            .collect::<Vec<_>>()
            .join(", ");
        if !defaults.is_empty() {
            let _ = writeln!(out, "    public {simple}() {{ this({defaults}); }}");
        }
    }
    let all = used.cloned().unwrap_or_default();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for method in &class.declared {
        if method.kind != MemberKind::Method || method.name == "<init>" {
            continue;
        }
        if class.name == "net/minecraft/util/StringRepresentable$StringRepresentableCodec"
            && matches!(method.name.as_str(), "encode" | "decode")
        {
            continue;
        }
        if is_record
            && class.declared.iter().any(|field| {
                field.kind == MemberKind::Field
                    && !field.is_static()
                    && field.name == method.name
                    && method.descriptor.starts_with("()")
            })
        {
            // Record components already generate their accessor methods.
            continue;
        }
        if is_record && matches!(method.name.as_str(), "equals" | "hashCode" | "toString") {
            // javac supplies the canonical record object methods.
            continue;
        }
        // `javac` writes these for an enum; declaring them is an error.
        if is_enum && (method.name == "values" || method.name == "valueOf") {
            continue;
        }
        let named = all.contains(&(method.name.clone(), method.descriptor.clone()));
        if !named
            && !(is_interface && (method.is_public() || method.is_protected()))
            && !(is_record && (method.is_public() || method.is_protected()))
        {
            continue;
        }
        if !(method.is_public() || method.is_protected()) {
            continue;
        }
        // An interface cannot redeclare `Object`'s methods as default.
        if is_interface && matches!(method.name.as_str(), "toString" | "equals" | "hashCode") {
            continue;
        }
        let (ret, params) = signature(&method.descriptor);
        // Two generic overloads erase to one signature; emit it once.
        if !seen.insert((method.name.clone(), params.clone())) {
            continue;
        }
        let statik = if method.is_static() { "static " } else { "" };
        let keyword = if is_interface && !method.is_static() {
            "default "
        } else {
            statik
        };
        // Preserve the callable visibility from the game shape. Leaving a
        // public method package-private compiles the façade but fails as soon
        // as a mod invokes it from its own package (`IllegalAccessError`).
        // Object overrides are public in the real classes as well.
        let visibility = if method.is_public() {
            "public "
        } else {
            "protected "
        };
        let body =
            runtime_body_with_descriptor(&class.name, &method.name, &ret, &method.descriptor);
        // A functional interface a mod hands a lambda to keeps exactly one
        // abstract method: `default` would leave no lambda target at all, and
        // an abstract method may not carry a body.
        if is_interface && functional_interface(&class.name) && !method.is_static() {
            let _ = writeln!(out, "    {visibility}{ret} {}({params});", method.name);
        } else {
            let _ = writeln!(
                out,
                "    {visibility}{keyword}{ret} {}({params}) {{ {body} }}",
                method.name
            );
        }
        methods += 1;
    }
    // The NeoForge item-handler shim is compiled beside the generated game
    // façade rather than being part of the mod census. Materialize the small
    // ItemStack value ABI it uses even when a mod only mentions ItemStack in
    // an IItemHandler signature; otherwise the shim's reflective access would
    // fail closed to an empty stack for every transfer.
    if class.name == "net/minecraft/world/item/ItemStack" {
        let required = [
            ("getCount", "()I", "int", ""),
            ("isDamageable", "()Z", "boolean", ""),
            (
                "getItem",
                "()Lnet/minecraft/world/item/Item;",
                "net.minecraft.world.item.Item",
                "",
            ),
            ("setCount", "(I)V", "void", "int p0"),
            (
                "copy",
                "()Lnet/minecraft/world/item/ItemStack;",
                "net.minecraft.world.item.ItemStack",
                "",
            ),
            (
                "copyWithCount",
                "(I)Lnet/minecraft/world/item/ItemStack;",
                "net.minecraft.world.item.ItemStack",
                "int p0",
            ),
            ("grow", "(I)V", "void", "int p0"),
            ("shrink", "(I)V", "void", "int p0"),
            ("isEmpty", "()Z", "boolean", ""),
        ];
        for (name, descriptor, ret, params) in required {
            if !seen.insert((name.to_owned(), params.to_owned())) {
                continue;
            }
            let body = runtime_body_with_descriptor(&class.name, name, ret, descriptor);
            let _ = writeln!(out, "    public {ret} {name}({params}) {{ {body} }}");
            methods += 1;
        }
    }
    if class.name == "net/minecraft/world/item/Item" {
        let required = [
            ("getMaxStackSize", "()I", "int"),
            ("getMaxDamage", "()I", "int"),
            ("isDamageable", "()Z", "boolean"),
            (
                "getCraftingRemainingItem",
                "()Lnet/minecraft/world/item/Item;",
                "net.minecraft.world.item.Item",
            ),
            ("isFireResistant", "()Z", "boolean"),
        ];
        for (name, descriptor, ret) in required {
            if !seen.insert((name.to_owned(), String::new())) {
                continue;
            }
            let body = runtime_body_with_descriptor(&class.name, name, ret, descriptor);
            let _ = writeln!(out, "    public {ret} {name}() {{ {body} }}");
            methods += 1;
        }
    }
    if class.name == "net/minecraft/world/entity/Entity" {
        let required = [
            ("getEyeHeight", "()F", "float"),
            ("getBbHeight", "()F", "float"),
            ("getBbWidth", "()F", "float"),
            ("getBlockX", "()I", "int"),
            ("getBlockY", "()I", "int"),
            ("getBlockZ", "()I", "int"),
            ("isPassenger", "()Z", "boolean"),
            (
                "getVehicle",
                "()Lnet/minecraft/world/entity/Entity;",
                "net.minecraft.world.entity.Entity",
            ),
        ];
        for (name, descriptor, ret) in required {
            if !seen.insert((name.to_owned(), String::new())) {
                continue;
            }
            let body = runtime_body_with_descriptor(&class.name, name, ret, descriptor);
            let _ = writeln!(out, "    public {ret} {name}() {{ {body} }}");
            methods += 1;
        }
    }
    // A support-only `BlockPos` travels with `Level`, whose bounded
    // `horizonBlockKey` reads its coordinates. The methods have to exist even
    // when the mod never names one itself.
    if class.name == "net/minecraft/core/BlockPos" {
        for (name, ret) in [("getX", "int"), ("getY", "int"), ("getZ", "int")] {
            if !seen.insert((name.to_owned(), String::new())) {
                continue;
            }
            let body = runtime_body_with_descriptor(&class.name, name, ret, "()I");
            let _ = writeln!(out, "    public {ret} {name}() {{ {body} }}");
            methods += 1;
        }
    }
    if class.name == "net/minecraft/world/level/block/state/BlockState" {
        for (name, descriptor, ret) in [
            (
                "getFluidState",
                "()Lnet/minecraft/world/level/material/FluidState;",
                "net.minecraft.world.level.material.FluidState",
            ),
            ("hasBlockEntity", "()Z", "boolean"),
            ("isRandomlyTicking", "()Z", "boolean"),
        ] {
            if !seen.insert((name.to_owned(), String::new())) {
                continue;
            }
            let body = runtime_body_with_descriptor(&class.name, name, ret, descriptor);
            let _ = writeln!(out, "    public {ret} {name}() {{ {body} }}");
            methods += 1;
        }
    }
    if class.name == "net/minecraft/network/chat/MutableComponent" {
        if seen.insert(("getString".to_owned(), "".to_owned())) {
            let _ = writeln!(
                out,
                "    public String getString() {{ return horizonText; }}"
            );
            methods += 1;
        }
        if seen.insert((
            "append".to_owned(),
            "net.minecraft.network.chat.Component p0".to_owned(),
        )) {
            let _ = writeln!(
                out,
                "    public net.minecraft.network.chat.MutableComponent append(net.minecraft.network.chat.Component p0) {{ if (p0 != null) horizonText += p0.getString(); return this; }}"
            );
            methods += 1;
        }
        if seen.insert(("append".to_owned(), "java.lang.String p0".to_owned())) {
            let _ = writeln!(
                out,
                "    public net.minecraft.network.chat.MutableComponent append(java.lang.String p0) {{ if (p0 != null) horizonText += p0; return this; }}"
            );
            methods += 1;
        }
        if seen.insert(("copy".to_owned(), "".to_owned())) {
            let _ = writeln!(
                out,
                "    public net.minecraft.network.chat.MutableComponent copy() {{ net.minecraft.network.chat.MutableComponent value = new net.minecraft.network.chat.MutableComponent(); value.horizonText = horizonText; value.horizonKey = horizonKey; return value; }}"
            );
            methods += 1;
        }
    }
    if is_record {
        let mut add_record_method = |name: &str, params: &str, ret: &str, body: &str| {
            if seen.insert((name.to_owned(), params.to_owned())) {
                let _ = writeln!(out, "    public {ret} {name}({params}) {{ {body} }}");
            }
        };
        for interface in &class.interfaces {
            match interface.as_str() {
                "java/lang/AutoCloseable" => add_record_method("close", "", "void", ""),
                "java/lang/Comparable" => {
                    add_record_method("compareTo", "java.lang.Object p0", "int", "return 0;")
                }
                "java/util/function/Predicate" => {
                    add_record_method("test", "java.lang.Object p0", "boolean", "return false;")
                }
                _ => {}
            }
        }
    }
    // A mod can be compiled against a nearby 26.3 snapshot whose member was
    // removed or renamed in the game jar. Preserve that named ABI edge as a
    // synthetic public method so the JVM reports the next semantic gap rather
    // than failing linkage before the entrypoint starts.
    for (name, descriptor) in &all {
        if name == "<init>"
            || matches!(name.as_str(), "getClass" | "name" | "ordinal")
            || (is_interface && matches!(name.as_str(), "equals" | "hashCode" | "toString"))
            || (is_enum
                && matches!(
                    name.as_str(),
                    "values" | "valueOf" | "hashCode" | "equals" | "toString"
                ))
            || class
                .declared
                .iter()
                .any(|member| member.name == *name && member.descriptor == *descriptor)
        {
            continue;
        }
        let (ret, params) = signature(descriptor);
        if !seen.insert((name.clone(), params.clone())) {
            continue;
        }
        let body = runtime_body_with_descriptor(&class.name, name, &ret, descriptor);
        let keyword = if is_interface { "default " } else { "" };
        let _ = writeln!(
            out,
            "    public {keyword}{ret} {name}({params}) {{ {body} }}"
        );
        methods += 1;
    }
    // Materialize the abstract methods from the safe accessor/invoker
    // interfaces on the generated target class. The bodies intentionally
    // return the JVM default for their primitive/void descriptor; this
    // provides linkage and casts while keeping semantic work visible at the
    // next probe boundary.
    if let Some(mixins) = mixin_impls {
        for mixin in mixins {
            if !available.contains(&mixin.interface) {
                continue;
            }
            for method in &mixin.methods {
                let (ret, params) = signature(&method.descriptor);
                if !seen.insert((method.name.clone(), params.clone())) {
                    continue;
                }
                let body = if ret == "void" {
                    String::new()
                } else {
                    format!("return {};", default_value_for_java_type(&ret))
                };
                let _ = writeln!(
                    out,
                    "    public {ret} {}({params}) {{ {body} }}",
                    method.name
                );
                methods += 1;
            }
        }
    }
    for child in inner {
        let _ = writeln!(out, "{child}");
    }
    let _ = writeln!(out, "}}");
    report.classes += 1;
    report.methods += methods;
    report.fields += fields;
    out
}

/// Small value factories that are safe to execute while a mod is booting.
///
/// The façade is intentionally mostly a shape-only jar, but a few classes are
/// reached from static initializers before the game has attached its native
/// bridge. `StreamCodec` is one such class: its factories only need to return
/// an object implementing the codec interface. Returning an empty façade
/// instance keeps registration code alive; encode/decode remain the next
/// boundary to implement when a mod actually sends data.
#[cfg(test)]
fn runtime_body(class: &str, method: &str, ret: &str) -> String {
    runtime_body_with_descriptor(class, method, ret, "")
}

fn runtime_body_with_descriptor(class: &str, method: &str, ret: &str, descriptor: &str) -> String {
    // NeoForge's public registration path is deliberately small: a mod gets
    // an event bus from the loading context, creates a DeferredRegister, and
    // attaches it to that bus. Keep those calls alive in the façade without
    // importing NeoForge's implementation.
    if class == "net/neoforged/fml/javafmlmod/FMLJavaModLoadingContext"
        && method == "get"
        && ret == "net.neoforged.fml.javafmlmod.FMLJavaModLoadingContext"
    {
        return "return new FMLJavaModLoadingContext();".to_owned();
    }
    if class == "net/neoforged/fml/javafmlmod/FMLJavaModLoadingContext"
        && method == "getModEventBus"
        && ret == "net.neoforged.bus.api.IEventBus"
    {
        return "return new net.neoforged.bus.api.IEventBus() {};".to_owned();
    }
    if class == "net/neoforged/fml/javafmlmod/FMLJavaModLoadingContext"
        && method == "getContainer"
        && ret == "net.neoforged.fml.ModContainer"
    {
        return "return new net.neoforged.fml.ModContainer();".to_owned();
    }
    if class == "net/neoforged/fml/ModContainer"
        && method == "getEventBus"
        && ret == "net.neoforged.bus.api.IEventBus"
    {
        return "return new net.neoforged.bus.api.IEventBus() {};".to_owned();
    }
    if class == "net/neoforged/bus/api/IEventBus" && ret == "net.neoforged.bus.api.IEventBus" {
        return "return this;".to_owned();
    }
    if class == "net/neoforged/bus/api/IEventBus"
        && method == "post"
        && ret == "net.neoforged.bus.api.Event"
    {
        return "return p0;".to_owned();
    }
    if class == "net/minecraftforge/eventbus/api/IEventBus"
        && ret == "net.minecraftforge.eventbus.api.IEventBus"
    {
        return "return this;".to_owned();
    }
    if class == "net/minecraftforge/eventbus/api/IEventBus" && method == "post" && ret == "boolean"
    {
        return "return false;".to_owned();
    }
    if matches!(
        class,
        "net/neoforged/neoforge/capabilities/CapabilityManager"
            | "net/minecraftforge/common/capabilities/CapabilityManager"
    ) && method == "get"
        && (ret == "net.neoforged.neoforge.capabilities.Capability"
            || ret == "net.minecraftforge.common.capabilities.Capability")
    {
        return format!(
            "return new {}.Capability();",
            class.replace("/CapabilityManager", "").replace('/', ".")
        );
    }
    if matches!(
        class,
        "net/neoforged/neoforge/capabilities/Capability"
            | "net/minecraftforge/common/capabilities/Capability"
    ) && ret == "boolean"
        && method == "isRegistered"
    {
        return "return true;".to_owned();
    }
    if matches!(
        class,
        "net/neoforged/neoforge/capabilities/Capability"
            | "net/minecraftforge/common/capabilities/Capability"
    ) && method == "orEmpty"
        && (ret == "net.neoforged.neoforge.common.util.LazyOptional"
            || ret == "net.minecraftforge.common.util.LazyOptional")
    {
        let optional = if class.starts_with("net/minecraftforge/") {
            "net.minecraftforge.common.util.LazyOptional"
        } else {
            "net.neoforged.neoforge.common.util.LazyOptional"
        };
        return format!(
            "return p1 == this && p0 != null ? {optional}.ofNullable(p0) : {optional}.empty();"
        );
    }
    if matches!(
        class,
        "net/neoforged/neoforge/common/util/LazyOptional"
            | "net/minecraftforge/common/util/LazyOptional"
    ) {
        match (method, ret) {
            ("empty", "net.neoforged.neoforge.common.util.LazyOptional")
            | ("of", "net.neoforged.neoforge.common.util.LazyOptional")
            | ("ofNullable", "net.neoforged.neoforge.common.util.LazyOptional") => {
                return "return new net.neoforged.neoforge.common.util.LazyOptional();".to_owned();
            }
            ("empty", "net.minecraftforge.common.util.LazyOptional")
            | ("of", "net.minecraftforge.common.util.LazyOptional")
            | ("ofNullable", "net.minecraftforge.common.util.LazyOptional") => {
                return "return new net.minecraftforge.common.util.LazyOptional();".to_owned();
            }
            ("isPresent", "boolean") => {
                return "return horizonValid && horizonGet() != null;".to_owned();
            }
            ("resolve", _) => return "return horizonGet();".to_owned(),
            ("orElse", _) => return "return horizonGet() == null ? p0 : horizonGet();".to_owned(),
            ("invalidate", "void") => {
                return "horizonValid = false; horizonValue = null; horizonSupplier = () -> null;"
                    .to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraftforge/common/util/LazyOptional" {
        match (method, ret) {
            ("empty", "net.minecraftforge.common.util.LazyOptional")
            | ("of", "net.minecraftforge.common.util.LazyOptional")
            | ("ofNullable", "net.minecraftforge.common.util.LazyOptional") => {
                return "return new net.minecraftforge.common.util.LazyOptional();".to_owned();
            }
            ("isPresent", "boolean") => {
                return "return horizonValid && horizonGet() != null;".to_owned();
            }
            ("resolve", _) => return "return horizonGet();".to_owned(),
            ("orElse", _) => return "return horizonGet() == null ? p0 : horizonGet();".to_owned(),
            ("invalidate", "void") => {
                return "horizonValid = false; horizonValue = null; horizonSupplier = () -> null;"
                    .to_owned();
            }
            _ => {}
        }
    }
    if class == "net/neoforged/neoforge/registries/RegisterEvent" {
        if method == "getRegistryKey" && ret == "net.minecraft.resources.ResourceKey" {
            return "return horizonRegistryKey;".to_owned();
        }
        if method == "getRegistry" && ret == "net.minecraft.core.Registry" {
            return "return horizonRegistry;".to_owned();
        }
    }
    if class == "net/neoforged/neoforge/registries/DeferredRegister" {
        if method == "create" && ret == "net.neoforged.neoforge.registries.DeferredRegister" {
            return "return new DeferredRegister();".to_owned();
        }
        if ret == "net.neoforged.neoforge.registries.DeferredRegister" {
            return "return this;".to_owned();
        }
        if ret == "net.neoforged.neoforge.registries.DeferredHolder" {
            return "return new net.neoforged.neoforge.registries.DeferredHolder();".to_owned();
        }
    }
    if class == "net/neoforged/neoforge/registries/DeferredHolder" {
        match (method, ret) {
            ("get" | "value", _) if descriptor == "()Ljava/lang/Object;" => {
                return "if (horizonValue == null && horizonSupplier != null) horizonValue = horizonSupplier.get(); return horizonValue;".to_owned();
            }
            ("isPresent", "boolean") => {
                return "return get() != null;".to_owned();
            }
            ("getOptional", "java.util.Optional") => {
                return "return java.util.Optional.ofNullable(get());".to_owned();
            }
            ("getKey", "net.minecraft.resources.ResourceKey") => {
                return "return horizonKey;".to_owned();
            }
            ("getId", "net.minecraft.resources.Identifier") => {
                return "return horizonId;".to_owned();
            }
            _ => {}
        }
    }
    // InteractionResult is a sealed value interface used by virtually every
    // item and block interaction callback. Its 26.3 shape is four nested
    // records (Success, Fail, Pass, TryEmptyHandInteraction), so preserve the
    // bounded result predicate and Success value accessors instead of treating
    // it as the old enum-shaped API.
    if class == "net/minecraft/world/InteractionResult"
        && let ("consumesAction", "boolean", _) = (method, ret, descriptor)
    {
        return "return this instanceof InteractionResult.Success;".to_owned();
    }
    // Bounded wire semantics for the generated `FriendlyByteBuf`: every
    // wire-shaped write encodes big-endian bytes into the growable in-memory
    // buffer injected beside the class fields, and every read decodes them at
    // a bounded cursor, so a mod's encode/decode pair round-trips without a
    // Netty buffer or packet framing. Values with no wire shape (NBT tags)
    // keep the `horizonValues` marker FIFO. Reads past the end answer the
    // primitive zero (or `""`) instead of throwing, because a façade buffer is
    // also handed to decoders the game fills elsewhere.
    if class == "net/minecraft/network/FriendlyByteBuf" {
        match (method, ret, descriptor) {
            // Primitives and varints.
            ("writeByte", _, "(I)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPut(p0); return this;".to_owned();
            }
            ("readByte", "byte", "()B") => return "return (byte) horizonGet();".to_owned(),
            ("readUnsignedByte", "short", "()S") => {
                return "return (short) horizonGet();".to_owned();
            }
            ("writeShort", _, "(I)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutShort(p0); return this;".to_owned();
            }
            ("writeShortLE", _, "(I)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutShortLE(p0); return this;".to_owned();
            }
            ("readShort", "short", "()S") => {
                return "return (short) horizonGetShort();".to_owned();
            }
            ("readShortLE", "short", "()S") => {
                return "return (short) horizonGetShortLE();".to_owned();
            }
            ("readUnsignedShort", "int", "()I") => {
                return "return horizonGetShort();".to_owned();
            }
            ("readUnsignedShortLE", "int", "()I") => {
                return "return horizonGetShortLE();".to_owned();
            }
            ("writeMedium", _, "(I)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutMedium(p0); return this;".to_owned();
            }
            ("writeMediumLE", _, "(I)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutMediumLE(p0); return this;".to_owned();
            }
            ("readMedium", "int", "()I") => return "return horizonGetMedium();".to_owned(),
            ("readMediumLE", "int", "()I") => {
                return "return horizonGetMediumLE();".to_owned();
            }
            ("writeChar", _, "(C)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutShort(p0); return this;".to_owned();
            }
            ("readChar", "char", "()C") => return "return (char) horizonGetShort();".to_owned(),
            ("writeInt", _, "(I)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutInt(p0); return this;".to_owned();
            }
            ("writeIntLE", _, "(I)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutIntLE(p0); return this;".to_owned();
            }
            ("readInt", "int", "()I") => return "return horizonGetInt();".to_owned(),
            ("readIntLE", "int", "()I") => return "return horizonGetIntLE();".to_owned(),
            ("writeVarInt", _, "(I)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutVarInt(p0); return this;".to_owned();
            }
            ("readVarInt", "int", "()I") => return "return horizonGetVarInt();".to_owned(),
            ("writeLong", _, "(J)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutLong(p0); return this;".to_owned();
            }
            ("writeLongLE", _, "(J)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutLongLE(p0); return this;".to_owned();
            }
            ("readLong", "long", "()J") => return "return horizonGetLong();".to_owned(),
            ("readLongLE", "long", "()J") => return "return horizonGetLongLE();".to_owned(),
            ("writeVarLong", _, "(J)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutVarLong(p0); return this;".to_owned();
            }
            ("readVarLong", "long", "()J") => return "return horizonGetVarLong();".to_owned(),
            ("writeFloat", _, "(F)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutInt(Float.floatToRawIntBits(p0)); return this;".to_owned();
            }
            ("readFloat", "float", "()F") => {
                return "return Float.intBitsToFloat(horizonGetInt());".to_owned();
            }
            ("writeDouble", _, "(D)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutLong(Double.doubleToRawLongBits(p0)); return this;".to_owned();
            }
            ("readDouble", "double", "()D") => {
                return "return Double.longBitsToDouble(horizonGetLong());".to_owned();
            }
            ("writeBoolean", _, "(Z)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPut(p0 ? 1 : 0); return this;".to_owned();
            }
            ("readBoolean", "boolean", "()Z") => {
                return "return horizonGet() != 0;".to_owned();
            }
            // Strings: a UTF-8 length-prefixed payload bounded by `maxLength`
            // characters (vanilla's 3-bytes-per-char worst case).
            ("writeUtf", _, "(Ljava/lang/String;)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutUtf(p0, 32767); return this;".to_owned();
            }
            ("writeUtf", _, "(Ljava/lang/String;I)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutUtf(p0, p1); return this;".to_owned();
            }
            ("readUtf", "java.lang.String", "()Ljava/lang/String;") => {
                return "return horizonGetUtf(32767);".to_owned();
            }
            ("readUtf", "java.lang.String", "(I)Ljava/lang/String;") => {
                return "return horizonGetUtf(p0);".to_owned();
            }
            ("writeString", _, "(Ljava/lang/String;)V") => {
                return "horizonPutUtf(p0, 32767);".to_owned();
            }
            ("writeString", _, "(Ljava/lang/String;)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutUtf(p0, 32767); return this;".to_owned();
            }
            ("readString", "java.lang.String", "()Ljava/lang/String;") => {
                return "return horizonGetUtf(32767);".to_owned();
            }
            // Identifiers (`ResourceLocation` before the 26.3 rename).
            (
                "writeResourceLocation" | "writeIdentifier",
                _,
                "(Lnet/minecraft/resources/Identifier;)Lnet/minecraft/network/FriendlyByteBuf;",
            ) => {
                return "horizonPutUtf(p0 == null ? \"\" : p0.toString(), 32767); return this;"
                    .to_owned();
            }
            (
                "readResourceLocation" | "readIdentifier",
                "net.minecraft.resources.Identifier",
                "()Lnet/minecraft/resources/Identifier;",
            ) => {
                return "String value = horizonGetUtf(32767); int separator = value.indexOf(':'); return separator < 0 ? new net.minecraft.resources.Identifier(\"minecraft\", value) : new net.minecraft.resources.Identifier(value.substring(0, separator), value.substring(separator + 1));"
                    .to_owned();
            }
            // Value objects.
            ("writeUUID", _, "(Ljava/util/UUID;)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutLong(p0.getMostSignificantBits()); horizonPutLong(p0.getLeastSignificantBits()); return this;"
                    .to_owned();
            }
            ("readUUID", "java.util.UUID", "()Ljava/util/UUID;") => {
                return "return new java.util.UUID(horizonGetLong(), horizonGetLong());".to_owned();
            }
            (
                "writeBlockPos",
                _,
                "(Lnet/minecraft/core/BlockPos;)Lnet/minecraft/network/FriendlyByteBuf;",
            ) => {
                return "horizonPutLong(((long) p0.getX() & 0x3FFFFFFL) << 38 | ((long) p0.getZ() & 0x3FFFFFFL) << 12 | ((long) p0.getY() & 0xFFFL)); return this;"
                    .to_owned();
            }
            ("readBlockPos", "net.minecraft.core.BlockPos", "()Lnet/minecraft/core/BlockPos;") => {
                return "long packed = horizonGetLong(); return new net.minecraft.core.BlockPos((int) (packed >> 38), (int) (packed << 52 >> 52), (int) (packed << 26 >> 38));"
                    .to_owned();
            }
            (
                "writeChunkPos",
                _,
                "(Lnet/minecraft/core/ChunkPos;)Lnet/minecraft/network/FriendlyByteBuf;",
            ) => {
                return "horizonPutInt(p0.x); horizonPutInt(p0.z); return this;".to_owned();
            }
            ("readChunkPos", "net.minecraft.core.ChunkPos", "()Lnet/minecraft/core/ChunkPos;") => {
                return "return new net.minecraft.core.ChunkPos(horizonGetInt(), horizonGetInt());"
                    .to_owned();
            }
            // Arrays: `writeByteArray` keeps the length prefix, `writeBytes`
            // is Netty's raw form with the same bounded buffer underneath.
            ("writeByteArray", _, "([B)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutVarInt(p0 == null ? 0 : p0.length); horizonPutBytes(p0); return this;"
                    .to_owned();
            }
            ("writeBytes", _, "([B)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutBytes(p0); return this;".to_owned();
            }
            ("readByteArray", "byte[]", "()[B") => {
                return "return horizonGetByteArray(HORIZON_MAX_BYTES);".to_owned();
            }
            ("readByteArray", "byte[]", "(I)[B") => {
                return "return horizonGetByteArray(p0);".to_owned();
            }
            ("writeBooleanArray", _, "([Z)V") => {
                return "horizonPutVarInt(p0 == null ? 0 : p0.length); if (p0 != null) for (boolean value : p0) horizonPut(value ? 1 : 0);".to_owned();
            }
            ("writeBooleanArray", _, "([Z)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutVarInt(p0 == null ? 0 : p0.length); if (p0 != null) for (boolean value : p0) horizonPut(value ? 1 : 0); return this;".to_owned();
            }
            ("readBooleanArray", "boolean[]", "()[Z") => {
                return "int length = horizonGetVarInt(); if (length < 0 || length > HORIZON_MAX_BYTES) throw new IllegalArgumentException(\"Invalid boolean array length: \" + length); if (length > horizonReadable()) return new boolean[0]; boolean[] data = new boolean[length]; for (int i = 0; i < length; i++) data[i] = horizonGet() != 0; return data;".to_owned();
            }
            // 26.3 writes an `int[]` as a VarInt count then VarInt elements
            // and calls it `writeVarIntArray`; `writeIntArray` is gone from
            // the real buffer. Keep the older spelling as an alias over the
            // same wire form so a jar naming either one round-trips.
            ("writeVarIntArray", _, "([I)V") | ("writeIntArray", _, "([I)V") => {
                return "horizonPutVarInt(p0 == null ? 0 : p0.length); if (p0 != null) for (int value : p0) horizonPutVarInt(value);".to_owned();
            }
            ("writeVarIntArray", _, "([I)Lnet/minecraft/network/FriendlyByteBuf;")
            | ("writeIntArray", _, "([I)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutVarInt(p0 == null ? 0 : p0.length); if (p0 != null) for (int value : p0) horizonPutVarInt(value); return this;".to_owned();
            }
            ("readVarIntArray", "int[]", "()[I") | ("readIntArray", "int[]", "()[I") => {
                return "int length = horizonGetVarInt(); if (length < 0 || length > HORIZON_MAX_BYTES) throw new IllegalArgumentException(\"Invalid int array length: \" + length); if (length > horizonReadable()) return new int[0]; int[] data = new int[length]; for (int i = 0; i < length; i++) data[i] = horizonGetVarInt(); return data;".to_owned();
            }
            ("writeLongArray", _, "([J)V") => {
                return "horizonPutVarInt(p0 == null ? 0 : p0.length); if (p0 != null) for (long value : p0) horizonPutLong(value);".to_owned();
            }
            ("writeLongArray", _, "([J)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutVarInt(p0 == null ? 0 : p0.length); if (p0 != null) for (long value : p0) horizonPutLong(value); return this;".to_owned();
            }
            ("readLongArray", "long[]", "()[J") => {
                return "int length = horizonGetVarInt(); if (length < 0 || length > HORIZON_MAX_BYTES / 8) throw new IllegalArgumentException(\"Invalid long array length: \" + length); if (length * 8 > horizonReadable()) return new long[0]; long[] data = new long[length]; for (int i = 0; i < length; i++) data[i] = horizonGetLong(); return data;".to_owned();
            }
            ("writeFloatArray", _, "([F)V") => {
                return "horizonPutVarInt(p0 == null ? 0 : p0.length); if (p0 != null) for (float value : p0) horizonPutInt(Float.floatToRawIntBits(value));".to_owned();
            }
            ("writeFloatArray", _, "([F)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutVarInt(p0 == null ? 0 : p0.length); if (p0 != null) for (float value : p0) horizonPutInt(Float.floatToRawIntBits(value)); return this;".to_owned();
            }
            ("readFloatArray", "float[]", "()[F") => {
                return "int length = horizonGetVarInt(); if (length < 0 || length > HORIZON_MAX_BYTES / 4) throw new IllegalArgumentException(\"Invalid float array length: \" + length); if (length * 4 > horizonReadable()) return new float[0]; float[] data = new float[length]; for (int i = 0; i < length; i++) data[i] = Float.intBitsToFloat(horizonGetInt()); return data;".to_owned();
            }
            ("writeDoubleArray", _, "([D)V") => {
                return "horizonPutVarInt(p0 == null ? 0 : p0.length); if (p0 != null) for (double value : p0) horizonPutLong(Double.doubleToRawLongBits(value));".to_owned();
            }
            ("writeDoubleArray", _, "([D)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutVarInt(p0 == null ? 0 : p0.length); if (p0 != null) for (double value : p0) horizonPutLong(Double.doubleToRawLongBits(value)); return this;".to_owned();
            }
            ("readDoubleArray", "double[]", "()[D") => {
                return "int length = horizonGetVarInt(); if (length < 0 || length > HORIZON_MAX_BYTES / 8) throw new IllegalArgumentException(\"Invalid double array length: \" + length); if (length * 8 > horizonReadable()) return new double[0]; double[] data = new double[length]; for (int i = 0; i < length; i++) data[i] = Double.longBitsToDouble(horizonGetLong()); return data;".to_owned();
            }
            ("writeStringArray", _, "([Ljava/lang/String;)V") => {
                return "horizonPutVarInt(p0 == null ? 0 : p0.length); if (p0 != null) for (String value : p0) horizonPutUtf(value, 32767);".to_owned();
            }
            (
                "writeStringArray",
                _,
                "([Ljava/lang/String;)Lnet/minecraft/network/FriendlyByteBuf;",
            ) => {
                return "horizonPutVarInt(p0 == null ? 0 : p0.length); if (p0 != null) for (String value : p0) horizonPutUtf(value, 32767); return this;".to_owned();
            }
            ("readStringArray", "java.lang.String[]", "()[Ljava/lang/String;") => {
                return "int length = horizonGetVarInt(); if (length < 0 || length > HORIZON_MAX_BYTES) throw new IllegalArgumentException(\"Invalid string array length: \" + length); String[] data = new String[length]; for (int i = 0; i < length; i++) data[i] = horizonGetUtf(32767); return data;".to_owned();
            }
            ("writeFloatLE", _, "(F)V") => {
                return "horizonPutIntLE(Float.floatToRawIntBits(p0));".to_owned();
            }
            ("writeFloatLE", _, "(F)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutIntLE(Float.floatToRawIntBits(p0)); return this;".to_owned();
            }
            ("readFloatLE", "float", "()F") => {
                return "return Float.intBitsToFloat(horizonGetIntLE());".to_owned();
            }
            ("writeDoubleLE", _, "(D)V") => {
                return "horizonPutLongLE(Double.doubleToRawLongBits(p0));".to_owned();
            }
            ("writeDoubleLE", _, "(D)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutLongLE(Double.doubleToRawLongBits(p0)); return this;".to_owned();
            }
            ("readDoubleLE", "double", "()D") => {
                return "return Double.longBitsToDouble(horizonGetLongLE());".to_owned();
            }
            // Enums travel as their ordinal VarInt, like vanilla.
            ("writeEnum", _, "(Ljava/lang/Enum;)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonPutVarInt(p0 == null ? -1 : p0.ordinal()); return this;".to_owned();
            }
            ("readEnum", "java.lang.Enum", "(Ljava/lang/Class;)Ljava/lang/Enum;") => {
                return "int ordinal = horizonGetVarInt(); if (p0 == null) return null; Object[] constants = p0.getEnumConstants(); return constants != null && ordinal >= 0 && ordinal < constants.length ? (java.lang.Enum) constants[ordinal] : null;"
                    .to_owned();
            }
            // Nullable/optional values write a presence byte and delegate the
            // value itself to the caller's bounded writer, so the pair
            // round-trips through the same cursor.
            (
                "writeNullable",
                _,
                "(Ljava/lang/Object;Ljava/util/function/BiConsumer;)Lnet/minecraft/network/FriendlyByteBuf;",
            ) => {
                return "if (p0 == null) { horizonPut(0); } else { horizonPut(1); p1.accept(this, p0); } return this;"
                    .to_owned();
            }
            (
                "readNullable",
                "java.lang.Object",
                "(Ljava/util/function/Function;)Ljava/lang/Object;",
            ) => {
                return "return horizonGet() == 0 ? null : p0.apply(this);".to_owned();
            }
            (
                "writeOptional",
                _,
                "(Ljava/util/Optional;Ljava/util/function/BiConsumer;)Lnet/minecraft/network/FriendlyByteBuf;",
            ) => {
                return "if (p0 == null || p0.isEmpty()) { horizonPut(0); } else { horizonPut(1); p1.accept(this, p0.get()); } return this;"
                    .to_owned();
            }
            (
                "readOptional",
                "java.util.Optional",
                "(Ljava/util/function/Function;)Ljava/util/Optional;",
            ) => {
                return "return horizonGet() == 0 ? java.util.Optional.empty() : java.util.Optional.ofNullable(p0.apply(this));"
                    .to_owned();
            }
            // Opaque markers: an NBT tag has no wire shape in the façade, so
            // it keeps the kind-checked FIFO and a null past the end.
            ("writeNbt", _, _) => return "horizonValues.add(p0); return this;".to_owned(),
            ("readNbt", _, _) => {
                return format!(
                    "return horizonReadIndex < horizonValues.size() ? ({ret}) horizonValues.get(horizonReadIndex++) : null;"
                );
            }
            // Bounded buffer counters.
            ("readerIndex", "int", "()I") => return "return horizonByteReadIndex;".to_owned(),
            ("readerIndex", _, "(I)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonByteReadIndex = Math.max(0, Math.min(p0, horizonWriteIndex)); return this;"
                    .to_owned();
            }
            ("readerIndex", "int", "(I)I") => {
                return "return horizonSetReaderIndexView(p0);".to_owned();
            }
            ("writerIndex", "int", "()I") => return "return horizonWriteIndex;".to_owned(),
            ("writerIndex", _, "(I)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonSetWriterIndex(p0); return this;".to_owned();
            }
            ("writerIndex", "int", "(I)I") => {
                return "return horizonSetWriterIndexView(p0);".to_owned();
            }
            ("setIndex", "void", "(I)V") => {
                return "horizonSetIndexView(p0);".to_owned();
            }
            ("readableBytes", "int", "()I") | ("remaining", "int", "()I") => {
                return "return horizonReadable();".to_owned();
            }
            ("isReadable", "boolean", "()Z") => {
                return "return horizonReadable() > 0;".to_owned();
            }
            ("isReadable", "boolean", "(I)Z") => {
                return "return horizonReadable() >= p0;".to_owned();
            }
            ("skipBytes", _, "(I)Lnet/minecraft/network/FriendlyByteBuf;") => {
                return "horizonSkip(p0); return this;".to_owned();
            }
            ("clear", _, _) => {
                return "horizonWriteIndex = 0; horizonByteReadIndex = 0; horizonBytes = new byte[0]; horizonValues.clear(); horizonReadIndex = 0; return this;"
                    .to_owned();
            }
            ("discardReadBytes", _, _) => return "horizonCompact(); return this;".to_owned(),
            _ => {}
        }
    }
    if class == "net/minecraft/world/InteractionResult$Success" {
        match (method, ret, descriptor) {
            ("consumesAction", "boolean", _) => return "return true;".to_owned(),
            ("shouldSwing", "boolean", _) => {
                return "return swingSource() != InteractionResult.SwingSource.NONE;".to_owned();
            }
            ("wasItemInteraction", "boolean", _) => {
                return "return itemContext().wasItemInteraction();".to_owned();
            }
            (
                "heldItemTransformedTo",
                "net.minecraft.world.item.ItemStack",
                "()Lnet/minecraft/world/item/ItemStack;",
            ) => {
                return "return itemContext().heldItemTransformedTo();".to_owned();
            }
            (
                "heldItemTransformedTo",
                "net.minecraft.world.InteractionResult.Success",
                "(Lnet/minecraft/world/item/ItemStack;)Lnet/minecraft/world/InteractionResult$Success;",
            ) => {
                return "return new Success(swingSource(), new InteractionResult.ItemContext(true, p0));".to_owned();
            }
            (
                "withoutItem",
                "net.minecraft.world.InteractionResult.Success",
                "()Lnet/minecraft/world/InteractionResult$Success;",
            ) => {
                return "return new Success(swingSource(), InteractionResult.ItemContext.NONE);"
                    .to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/world/InteractionResult$ItemContext" {
        match (method, ret, descriptor) {
            ("wasItemInteraction", "boolean", _) => {
                return "return wasItemInteraction;".to_owned();
            }
            (
                "heldItemTransformedTo",
                "net.minecraft.world.item.ItemStack",
                "()Lnet/minecraft/world/item/ItemStack;",
            ) => {
                return "return heldItemTransformedTo;".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/neoforged/neoforge/network/event/RegisterPayloadHandlersEvent"
        && method == "registrar"
        && ret == "net.neoforged.neoforge.network.registration.PayloadRegistrar"
    {
        return "return new net.neoforged.neoforge.network.registration.PayloadRegistrar();"
            .to_owned();
    }
    if class == "net/neoforged/neoforge/network/registration/PayloadRegistrar"
        && ret == "net.neoforged.neoforge.network.registration.PayloadRegistrar"
    {
        return "return this;".to_owned();
    }
    // Legacy Forge's SimpleChannel registration has no StreamCodec and carries
    // separate encoder/decoder callbacks. Reuse the NeoForge registrar marker
    // only for the bounded registration observation; byte encoding, channel
    // ownership and sendToServer/send remain unsupported.
    if class == "net/minecraftforge/network/NetworkRegistry"
        && method == "newSimpleChannel"
        && ret == "net.minecraftforge.network.simple.SimpleChannel"
    {
        return "return new net.minecraftforge.network.simple.SimpleChannel();".to_owned();
    }
    if class == "net/minecraftforge/network/simple/SimpleChannel"
        && method == "registerMessage"
        && ret == "net.minecraftforge.network.simple.SimpleChannel"
    {
        return "return this;".to_owned();
    }
    if class == "net/neoforged/fml/ModContainer"
        && method == "getModId"
        && ret == "java.lang.String"
    {
        return "return \"\";".to_owned();
    }
    if class == "net/minecraft/world/level/block/Block"
        && method == "column"
        && ret == "net.minecraft.world.phys.shapes.VoxelShape"
    {
        return voxel_shape_placeholder();
    }
    if matches!(
        class,
        "net/minecraft/core/Vec3i" | "net/minecraft/core/BlockPos"
    ) {
        match (method, ret) {
            ("getX", "int") => return "return horizonX;".to_owned(),
            ("getY", "int") => return "return horizonY;".to_owned(),
            ("getZ", "int") => return "return horizonZ;".to_owned(),
            ("offset", "net.minecraft.core.BlockPos")
                if descriptor == "(III)Lnet/minecraft/core/BlockPos;" =>
            {
                return "return new net.minecraft.core.BlockPos(horizonX + p0, horizonY + p1, horizonZ + p2);"
                    .to_owned();
            }
            ("relative", "net.minecraft.core.BlockPos")
                if descriptor
                    == "(Lnet/minecraft/core/Direction;)Lnet/minecraft/core/BlockPos;" =>
            {
                return "return new net.minecraft.core.BlockPos(horizonX + p0.getStepX(), horizonY + p0.getStepY(), horizonZ + p0.getStepZ());"
                    .to_owned();
            }
            ("subtract", "net.minecraft.core.Vec3i")
                if descriptor == "(Lnet/minecraft/core/Vec3i;)Lnet/minecraft/core/Vec3i;" =>
            {
                return "return new net.minecraft.core.Vec3i(horizonX - p0.getX(), horizonY - p0.getY(), horizonZ - p0.getZ());"
                    .to_owned();
            }
            ("distSqr", "double") if descriptor == "(Lnet/minecraft/core/Vec3i;)D" => {
                return "double dx = horizonX - p0.getX(); double dy = horizonY - p0.getY(); double dz = horizonZ - p0.getZ(); return dx * dx + dy * dy + dz * dz;"
                    .to_owned();
            }
            ("distSqr", "double") if descriptor == "(DDD)D" => {
                return "double dx = horizonX - p0; double dy = horizonY - p1; double dz = horizonZ - p2; return dx * dx + dy * dy + dz * dz;"
                    .to_owned();
            }
            ("closerThan", "boolean") if descriptor == "(Lnet/minecraft/core/Vec3i;D)Z" => {
                return "return distSqr(p0) < p1 * p1;".to_owned();
            }
            ("above", "net.minecraft.core.BlockPos") => {
                return "return new net.minecraft.core.BlockPos(horizonX, horizonY + 1, horizonZ);"
                    .to_owned();
            }
            ("below", "net.minecraft.core.BlockPos") => {
                return "return new net.minecraft.core.BlockPos(horizonX, horizonY - 1, horizonZ);"
                    .to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/world/phys/Vec3" {
        match (method, ret) {
            ("x", "double") => return "return horizonX;".to_owned(),
            ("y", "double") => return "return horizonY;".to_owned(),
            ("z", "double") => return "return horizonZ;".to_owned(),
            ("add", "net.minecraft.world.phys.Vec3")
                if descriptor
                    == "(Lnet/minecraft/world/phys/Vec3;)Lnet/minecraft/world/phys/Vec3;" =>
            {
                return "return new net.minecraft.world.phys.Vec3(horizonX + p0.x(), horizonY + p0.y(), horizonZ + p0.z());".to_owned();
            }
            ("add", "net.minecraft.world.phys.Vec3")
                if descriptor == "(DDD)Lnet/minecraft/world/phys/Vec3;" =>
            {
                return "return new net.minecraft.world.phys.Vec3(horizonX + p0, horizonY + p1, horizonZ + p2);".to_owned();
            }
            ("subtract", "net.minecraft.world.phys.Vec3")
                if descriptor
                    == "(Lnet/minecraft/world/phys/Vec3;)Lnet/minecraft/world/phys/Vec3;" =>
            {
                return "return new net.minecraft.world.phys.Vec3(horizonX - p0.x(), horizonY - p0.y(), horizonZ - p0.z());".to_owned();
            }
            ("subtract", "net.minecraft.world.phys.Vec3")
                if descriptor == "(DDD)Lnet/minecraft/world/phys/Vec3;" =>
            {
                return "return new net.minecraft.world.phys.Vec3(horizonX - p0, horizonY - p1, horizonZ - p2);".to_owned();
            }
            ("scale", "net.minecraft.world.phys.Vec3")
                if descriptor == "(D)Lnet/minecraft/world/phys/Vec3;" =>
            {
                return "return new net.minecraft.world.phys.Vec3(horizonX * p0, horizonY * p0, horizonZ * p0);".to_owned();
            }
            ("reverse", "net.minecraft.world.phys.Vec3") => {
                return "return new net.minecraft.world.phys.Vec3(-horizonX, -horizonY, -horizonZ);".to_owned();
            }
            ("lengthSqr", "double") => {
                return "return horizonX * horizonX + horizonY * horizonY + horizonZ * horizonZ;"
                    .to_owned();
            }
            ("length", "double") => {
                return "return Math.sqrt(horizonX * horizonX + horizonY * horizonY + horizonZ * horizonZ);".to_owned();
            }
            ("normalize", "net.minecraft.world.phys.Vec3") => {
                return "double length = length(); return length < 1.0E-4 ? new net.minecraft.world.phys.Vec3(0.0, 0.0, 0.0) : new net.minecraft.world.phys.Vec3(horizonX / length, horizonY / length, horizonZ / length);".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/world/phys/AABB" {
        match (method, ret) {
            ("getXsize", "double") => {
                return "return horizonMaxX - horizonMinX;".to_owned();
            }
            ("getYsize", "double") => {
                return "return horizonMaxY - horizonMinY;".to_owned();
            }
            ("getZsize", "double") => {
                return "return horizonMaxZ - horizonMinZ;".to_owned();
            }
            ("contains", "boolean") if descriptor == "(Lnet/minecraft/world/phys/Vec3;)Z" => {
                return "return p0.x() >= horizonMinX && p0.x() < horizonMaxX && p0.y() >= horizonMinY && p0.y() < horizonMaxY && p0.z() >= horizonMinZ && p0.z() < horizonMaxZ;".to_owned();
            }
            ("getCenter", "net.minecraft.world.phys.Vec3") => {
                return "return new net.minecraft.world.phys.Vec3((horizonMinX + horizonMaxX) * 0.5, (horizonMinY + horizonMaxY) * 0.5, (horizonMinZ + horizonMaxZ) * 0.5);".to_owned();
            }
            ("getBottomCenter", "net.minecraft.world.phys.Vec3") => {
                return "return new net.minecraft.world.phys.Vec3((horizonMinX + horizonMaxX) * 0.5, horizonMinY, (horizonMinZ + horizonMaxZ) * 0.5);".to_owned();
            }
            ("getMinPosition", "net.minecraft.world.phys.Vec3") => {
                return "return new net.minecraft.world.phys.Vec3(horizonMinX, horizonMinY, horizonMinZ);".to_owned();
            }
            ("getMaxPosition", "net.minecraft.world.phys.Vec3") => {
                return "return new net.minecraft.world.phys.Vec3(horizonMaxX, horizonMaxY, horizonMaxZ);".to_owned();
            }
            ("inflate", "net.minecraft.world.phys.AABB")
                if descriptor == "(D)Lnet/minecraft/world/phys/AABB;" =>
            {
                return "return new net.minecraft.world.phys.AABB(horizonMinX - p0, horizonMinY - p0, horizonMinZ - p0, horizonMaxX + p0, horizonMaxY + p0, horizonMaxZ + p0);".to_owned();
            }
            ("expandTowards", "net.minecraft.world.phys.AABB")
                if descriptor
                    == "(Lnet/minecraft/world/phys/Vec3;)Lnet/minecraft/world/phys/AABB;" =>
            {
                return "return new net.minecraft.world.phys.AABB(p0.x() < 0.0 ? horizonMinX + p0.x() : horizonMinX, p0.y() < 0.0 ? horizonMinY + p0.y() : horizonMinY, p0.z() < 0.0 ? horizonMinZ + p0.z() : horizonMinZ, p0.x() > 0.0 ? horizonMaxX + p0.x() : horizonMaxX, p0.y() > 0.0 ? horizonMaxY + p0.y() : horizonMaxY, p0.z() > 0.0 ? horizonMaxZ + p0.z() : horizonMaxZ);".to_owned();
            }
            ("ofSize", "net.minecraft.world.phys.AABB")
                if descriptor
                    == "(Lnet/minecraft/world/phys/Vec3;DDD)Lnet/minecraft/world/phys/AABB;" =>
            {
                return "return new net.minecraft.world.phys.AABB(p0.x() - p1 * 0.5, p0.y() - p2 * 0.5, p0.z() - p3 * 0.5, p0.x() + p1 * 0.5, p0.y() + p2 * 0.5, p0.z() + p3 * 0.5);".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/world/phys/HitResult" {
        match (method, ret) {
            ("getLocation", "net.minecraft.world.phys.Vec3") => {
                return "return horizonLocation;".to_owned();
            }
            ("getDirection", "net.minecraft.core.Direction") => {
                return "return horizonDirection;".to_owned();
            }
            ("isInside", "boolean") => return "return horizonInside;".to_owned(),
            _ => {}
        }
    }
    if class == "net/minecraft/world/phys/BlockHitResult" {
        match (method, ret, descriptor) {
            ("getBlockPos", "net.minecraft.core.BlockPos", "()Lnet/minecraft/core/BlockPos;") => {
                return "return horizonBlockPos;".to_owned();
            }
            (
                "withDirection",
                "net.minecraft.world.phys.BlockHitResult",
                "(Lnet/minecraft/core/Direction;)Lnet/minecraft/world/phys/BlockHitResult;",
            ) => {
                return "return new BlockHitResult(horizonLocation, p0, horizonBlockPos, horizonInside);".to_owned();
            }
            (
                "withPosition",
                "net.minecraft.world.phys.BlockHitResult",
                "(Lnet/minecraft/core/BlockPos;)Lnet/minecraft/world/phys/BlockHitResult;",
            ) => {
                return "return new BlockHitResult(horizonLocation, horizonDirection, p0, horizonInside);".to_owned();
            }
            (
                "miss",
                "net.minecraft.world.phys.BlockHitResult",
                "(Lnet/minecraft/world/phys/Vec3;Lnet/minecraft/core/Direction;Lnet/minecraft/core/BlockPos;)Lnet/minecraft/world/phys/BlockHitResult;",
            ) => {
                return "return new BlockHitResult(p0, p1, p2, false);".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/core/Direction" {
        match (method, ret, descriptor) {
            ("getName" | "getSerializedName", "java.lang.String", _) => {
                return "return name().toLowerCase(java.util.Locale.ROOT);".to_owned();
            }
            ("get3DDataValue", "int", _) => return "return ordinal();".to_owned(),
            ("getStepX", "int", _) => {
                return "return this == WEST ? -1 : this == EAST ? 1 : 0;".to_owned();
            }
            ("getStepY", "int", _) => {
                return "return this == DOWN ? -1 : this == UP ? 1 : 0;".to_owned();
            }
            ("getStepZ", "int", _) => {
                return "return this == NORTH ? -1 : this == SOUTH ? 1 : 0;".to_owned();
            }
            ("getAxis", "net.minecraft.core.Direction.Axis", _) => {
                return "return (this == UP || this == DOWN) ? Axis.Y : (this == EAST || this == WEST) ? Axis.X : Axis.Z;".to_owned();
            }
            ("getAxisDirection", "net.minecraft.core.Direction.AxisDirection", _) => {
                return "return (this == UP || this == SOUTH || this == EAST) ? AxisDirection.POSITIVE : AxisDirection.NEGATIVE;".to_owned();
            }
            ("getOpposite", "net.minecraft.core.Direction", _) => {
                return "return switch (this) { case DOWN -> UP; case UP -> DOWN; case NORTH -> SOUTH; case SOUTH -> NORTH; case WEST -> EAST; case EAST -> WEST; };".to_owned();
            }
            ("getClockWise", "net.minecraft.core.Direction", _) => {
                return "return switch (this) { case NORTH -> EAST; case EAST -> SOUTH; case SOUTH -> WEST; case WEST -> NORTH; default -> this; };".to_owned();
            }
            ("getCounterClockWise", "net.minecraft.core.Direction", _) => {
                return "return switch (this) { case NORTH -> WEST; case WEST -> SOUTH; case SOUTH -> EAST; case EAST -> NORTH; default -> this; };".to_owned();
            }
            ("getNormal", "net.minecraft.core.Vec3i", _) => {
                return "return new net.minecraft.core.Vec3i(getStepX(), getStepY(), getStepZ());"
                    .to_owned();
            }
            ("from3DDataValue", "net.minecraft.core.Direction", _) => {
                return "return values()[Math.floorMod(p0, values().length)];".to_owned();
            }
            ("byName", "net.minecraft.core.Direction", _) => {
                return "if (p0 == null) return null; for (Direction value : values()) if (value.getName().equals(p0)) return value; return null;".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/core/Direction$Axis" {
        match (method, ret) {
            ("getName" | "getSerializedName", "java.lang.String") => {
                return "return name().toLowerCase(java.util.Locale.ROOT);".to_owned();
            }
            ("isVertical", "boolean") => return "return this == Y;".to_owned(),
            ("isHorizontal", "boolean") => return "return this != Y;".to_owned(),
            ("getPlane", "net.minecraft.core.Direction.Plane") => {
                return "return this == Y ? Direction.Plane.VERTICAL : Direction.Plane.HORIZONTAL;"
                    .to_owned();
            }
            ("choose", "int") => {
                return "return this == X ? p0 : this == Y ? p1 : p2;".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/core/Direction$AxisDirection" {
        match (method, ret) {
            ("getStep", "int") => return "return this == POSITIVE ? 1 : -1;".to_owned(),
            ("isPositive", "boolean") => return "return this == POSITIVE;".to_owned(),
            ("getName" | "getSerializedName", "java.lang.String") => {
                return "return name().toLowerCase(java.util.Locale.ROOT);".to_owned();
            }
            ("opposite", "net.minecraft.core.Direction.AxisDirection") => {
                return "return this == POSITIVE ? NEGATIVE : POSITIVE;".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/world/InteractionHand" {
        match (method, ret) {
            ("getName", "java.lang.String") => {
                return "return this == MAIN_HAND ? \"main_hand\" : \"off_hand\";".to_owned();
            }
            ("getOpposite", "net.minecraft.world.InteractionHand") => {
                return "return this == MAIN_HAND ? OFF_HAND : MAIN_HAND;".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/network/protocol/common/custom/CustomPacketPayload$Type"
        && method == "id"
        && ret == "net.minecraft.resources.Identifier"
    {
        return "return horizonId;".to_owned();
    }
    if class == "net/minecraft/network/protocol/common/custom/CustomPacketPayload$Type" {
        match (method, ret, descriptor) {
            ("toString", "java.lang.String", "()Ljava/lang/String;") => {
                return "return horizonId.toString();".to_owned();
            }
            ("equals", "boolean", "(Ljava/lang/Object;)Z") => {
                return "return horizonId.equals(p0);".to_owned();
            }
            ("hashCode", "int", "()I") => {
                return "return horizonId.hashCode();".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/network/codec/StreamCodec" {
        if let Some(body) = stream_codec_body(method, descriptor) {
            return body;
        }
    }
    // `ByteBufCodecs`' factories build real codecs over the bounded
    // `FriendlyByteBuf`: a payload that writes its fields through a primitive
    // codec, a list or an optional round-trips them on the game side.
    if class == "net/minecraft/network/codec/ByteBufCodecs"
        && let Some(factory) = byte_buf_codec_factory(method, descriptor)
    {
        return factory;
    }
    // Buffer-wrapping calls answer the bounded buffer itself.
    if ret == "net.minecraft.network.FriendlyByteBuf"
        && matches!(
            class,
            "net/minecraft/network/FriendlyByteBuf"
                | "net/minecraft/network/VarInt"
                | "net/minecraft/network/codec/ByteBufCodecs"
        )
    {
        return "return p0;".to_owned();
    }
    // Codec values are used while Minecraft registers codecs in static
    // initializers.  The real datafixerupper classes are on the probe's
    // classpath, so a stable primitive codec is enough to let that bootstrap
    // continue without pretending to implement the encoded schema.
    if ret == "com.mojang.serialization.Codec" {
        return "return com.mojang.serialization.Codec.STRING;".to_owned();
    }
    if ret == "com.mojang.serialization.MapCodec" {
        return "return com.mojang.serialization.Codec.STRING.fieldOf(\"\");".to_owned();
    }
    if class == "net/minecraft/network/VarInt" && method == "getByteSize" && ret == "int" {
        return "int v = p0; return (v & -128) == 0 ? 1 : (v & -16384) == 0 ? 2 : (v & -2097152) == 0 ? 3 : (v & -268435456) == 0 ? 4 : 5;".to_owned();
    }
    if class == "net/minecraft/network/VarInt" && method == "hasContinuationBit" && ret == "boolean"
    {
        return "return (p0 & 0x80) != 0;".to_owned();
    }
    if class == "net/minecraft/world/flag/FeatureFlagSet"
        && method == "of"
        && ret == "net.minecraft.world.flag.FeatureFlagSet"
    {
        return "return new FeatureFlagSet();".to_owned();
    }
    if class == "net/minecraft/util/StringRepresentable" {
        if method == "fromEnum" && ret == "net.minecraft.util.StringRepresentable.EnumCodec" {
            return "return new EnumCodec();".to_owned();
        }
        if matches!(method, "fromValues" | "fromEnumWithMapping")
            && ret == "com.mojang.serialization.Codec"
        {
            return "return com.mojang.serialization.Codec.STRING;".to_owned();
        }
    }
    if class == "net/minecraft/core/Vec3i"
        && method == "offsetCodec"
        && ret == "com.mojang.serialization.Codec"
    {
        return "return com.mojang.serialization.Codec.STRING;".to_owned();
    }
    if class == "net/minecraft/resources/FileToIdConverter"
        && matches!(method, "json" | "registry")
        && ret == "net.minecraft.resources.FileToIdConverter"
    {
        return "return new FileToIdConverter();".to_owned();
    }
    if class == "net/minecraft/network/protocol/common/custom/CustomPacketPayload$Type"
        && method == "id"
        && ret == "net.minecraft.resources.Identifier"
    {
        return "return new net.minecraft.resources.Identifier();".to_owned();
    }
    if class == "net/minecraft/core/component/DataComponentType" && method == "builder" {
        return "return new DataComponentType.Builder();".to_owned();
    }
    if class == "net/minecraft/core/component/DataComponentType$Builder" {
        if ret == "net.minecraft.core.component.DataComponentType.Builder" {
            return "return this;".to_owned();
        }
        if method == "build" && ret == "net.minecraft.core.component.DataComponentType" {
            return "return new net.minecraft.core.component.DataComponentType() {};".to_owned();
        }
    }
    if matches!(
        class,
        "net/minecraft/core/Registry" | "net/minecraft/core/DefaultedRegistry"
    ) {
        if method == "register" {
            if descriptor
                .starts_with("(Lnet/minecraft/core/Registry;Lnet/minecraft/resources/ResourceKey;")
            {
                return "p0.horizonEntries.put(p1.location(), p2); p0.horizonKeys.put(p1.location(), p1); return p2;".to_owned();
            }
            if descriptor.starts_with("(Lnet/minecraft/resources/ResourceKey;") {
                return "horizonEntries.put(p1, p2); horizonKeys.put(p1, p0); return p2;"
                    .to_owned();
            }
            return "return p2;".to_owned();
        }
        if method == "containsKey" && ret == "boolean" {
            return "return horizonEntries.containsKey(p0);".to_owned();
        }
        if method == "get" && descriptor.contains("Lnet/minecraft/resources/ResourceKey;") {
            return format!("return ({ret}) horizonEntries.get(p0.location());");
        }
        if method == "get" && descriptor.contains("Lnet/minecraft/resources/Identifier;") {
            return format!("return ({ret}) horizonEntries.get(p0);");
        }
        if method == "getOrThrow"
            && (descriptor.contains("Lnet/minecraft/resources/ResourceKey;")
                || descriptor.contains("Lnet/minecraft/resources/Identifier;"))
        {
            let key = if descriptor.contains("Lnet/minecraft/resources/ResourceKey;") {
                "p0.location()"
            } else {
                "p0"
            };
            return format!(
                "Object value = horizonEntries.get({key}); if (value == null) throw new IllegalStateException(\"missing registry value: \" + {key}); return ({ret}) value;"
            );
        }
        if method == "getHolder"
            && ret == "java.util.Optional"
            && descriptor.contains("Lnet/minecraft/resources/ResourceKey;")
        {
            return "Object value = horizonEntries.get(p0.location()); if (value == null) return java.util.Optional.empty(); net.minecraft.core.Holder.Reference holder = new net.minecraft.core.Holder.Reference(); holder.horizonBind(value, p0, p0.location()); return java.util.Optional.of(holder);".to_owned();
        }
        if method == "getOptional"
            && ret == "java.util.Optional"
            && descriptor.contains("Lnet/minecraft/resources/Identifier;")
        {
            return "return java.util.Optional.ofNullable(horizonEntries.get(p0));".to_owned();
        }
        if method == "getKey" && ret == "net.minecraft.resources.Identifier" {
            return "for (java.util.Map.Entry<net.minecraft.resources.Identifier,Object> e : horizonEntries.entrySet()) if (java.util.Objects.equals(e.getValue(), p0)) return e.getKey(); return null;".to_owned();
        }
        if method == "getId" && ret == "int" {
            return "int id = 0; for (Object value : horizonEntries.values()) { if (java.util.Objects.equals(value, p0)) return id; id++; } return -1;".to_owned();
        }
        if method == "byId" {
            return format!(
                "int id = p0; if (id < 0 || id >= horizonEntries.size()) return null; return ({ret}) horizonEntries.values().toArray()[id];"
            );
        }
        if method == "size" && ret == "int" {
            return "return horizonEntries.size();".to_owned();
        }
        if method == "entrySet" && ret == "java.util.Set" {
            return "return horizonEntries.entrySet();".to_owned();
        }
        if method == "keySet" && ret == "java.util.Set" {
            return "return horizonEntries.keySet();".to_owned();
        }
        if method == "stream" && ret == "java.util.stream.Stream" {
            return "return horizonEntries.values().stream();".to_owned();
        }
        if method == "getResourceKey" && ret == "java.util.Optional" {
            return "net.minecraft.resources.Identifier id = getKey(p0); return id == null ? java.util.Optional.empty() : java.util.Optional.ofNullable(horizonKeys.get(id));".to_owned();
        }
    }
    if class == "net/minecraft/resources/RegistryValidator"
        && matches!(method, "none" | "nonEmpty")
        && ret == "net.minecraft.resources.RegistryValidator"
    {
        return "return new net.minecraft.resources.RegistryValidator() {};".to_owned();
    }
    if class == "net/minecraft/tags/TagKey"
        && method == "create"
        && ret == "net.minecraft.tags.TagKey"
    {
        return "return new net.minecraft.tags.TagKey(p0, p1);".to_owned();
    }
    if class == "net/minecraft/tags/TagKey" {
        // 26.3 makes `TagKey` a record with `registry` and `location`
        // components, so the bounded bodies read the components a record
        // accessor already supplies rather than something the façade declares.
        match (method, ret) {
            ("location", "net.minecraft.resources.Identifier") => {
                return "return location;".to_owned();
            }
            ("registry", "net.minecraft.resources.ResourceKey") => {
                return "return registry;".to_owned();
            }
            ("isFor", "boolean") => {
                return "return java.util.Objects.equals(registry, p0);".to_owned();
            }
            ("hashCode", "int") => {
                return "return java.util.Objects.hash(registry, location);".to_owned();
            }
            ("equals", "boolean") => {
                return "return p0 instanceof TagKey other && java.util.Objects.equals(registry, other.registry) && java.util.Objects.equals(location, other.location);".to_owned();
            }
            ("toString", "java.lang.String") => {
                return "return \"TagKey[\" + registry + \" / \" + location + \"]\";".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/core/HolderSet" {
        match (method, ret) {
            ("size", "int") => return "return 0;".to_owned(),
            ("isEmpty", "boolean") => return "return true;".to_owned(),
            ("contains", "boolean") => return "return false;".to_owned(),
            ("stream", "java.util.stream.Stream") => {
                return "return java.util.stream.Stream.empty();".to_owned();
            }
            ("get", _) => return "return null;".to_owned(),
            ("unwrap", "java.util.Optional") => {
                return "return java.util.Optional.empty();".to_owned();
            }
            ("canSerializeIn", "boolean") => return "return false;".to_owned(),
            ("empty" | "list" | "named", "net.minecraft.core.HolderSet") => {
                return "return new net.minecraft.core.HolderSet() {};".to_owned();
            }
            ("direct", "net.minecraft.core.HolderSet")
                if descriptor == "(Lnet/minecraft/core/Holder;)Lnet/minecraft/core/HolderSet;" =>
            {
                return "return new net.minecraft.core.HolderSet() { public int size() { return 1; } public boolean isEmpty() { return false; } public boolean contains(net.minecraft.core.Holder value) { return value == p0; } public java.util.stream.Stream stream() { return java.util.stream.Stream.of(p0); } public net.minecraft.core.Holder get(int index) { return index == 0 ? p0 : null; } };".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/core/Registry"
        && method == "registerForHolder"
        && ret == "net.minecraft.core.Holder.Reference"
    {
        if descriptor.starts_with(
            "(Lnet/minecraft/core/Registry;Lnet/minecraft/resources/ResourceKey;Lnet/minecraft/resources/Identifier;"
        ) {
            return "p0.horizonEntries.put(p2, p3); p0.horizonKeys.put(p2, p1); net.minecraft.core.Holder.Reference holder = new net.minecraft.core.Holder.Reference(); holder.horizonBind(p3, p1, p2); return holder;".to_owned();
        }
        return "horizonEntries.put(p1, p2); horizonKeys.put(p1, p0); net.minecraft.core.Holder.Reference holder = new net.minecraft.core.Holder.Reference(); holder.horizonBind(p2, p0, p1); return holder;".to_owned();
    }
    if class.starts_with("net/minecraft/core/Holder$") {
        match (method, ret) {
            ("value", _) => return "return horizonValue;".to_owned(),
            ("key", "net.minecraft.resources.ResourceKey") => {
                return "return horizonKey;".to_owned();
            }
            ("unwrapKey", "java.util.Optional") => {
                return "return java.util.Optional.ofNullable(horizonKey);".to_owned();
            }
            ("isBound", "boolean") => return "return horizonValue != null;".to_owned(),
            ("is", "boolean") if descriptor.contains("ResourceKey") => {
                return "return java.util.Objects.equals(horizonKey, p0);".to_owned();
            }
            ("is", "boolean") if descriptor.contains("Holder") => {
                return "return this == p0;".to_owned();
            }
            ("canSerializeIn", "boolean") => {
                return "return horizonValue != null;".to_owned();
            }
            ("kind", _) => return "return null;".to_owned(),
            ("tags", _) if ret == "java.util.stream.Stream" => {
                return "return java.util.stream.Stream.empty();".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/core/Holder" {
        match (method, ret) {
            ("value", _) => return "return null;".to_owned(),
            ("unwrapKey", "java.util.Optional") => {
                return "return java.util.Optional.empty();".to_owned();
            }
            ("isBound", "boolean") | ("is", "boolean") => {
                return "return false;".to_owned();
            }
            ("canSerializeIn", "boolean") => return "return false;".to_owned(),
            ("kind", _) => return "return null;".to_owned(),
            ("tags", _) if ret == "java.util.stream.Stream" => {
                return "return java.util.stream.Stream.empty();".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/resources/ResourceKey"
        && matches!(method, "create" | "createRegistryKey")
        && ret == "net.minecraft.resources.ResourceKey"
    {
        if method == "create" {
            return "return new ResourceKey(p0 == null ? new net.minecraft.resources.Identifier() : p0.identifier(), p1);".to_owned();
        }
        return "return new ResourceKey(new net.minecraft.resources.Identifier(), p0);".to_owned();
    }
    if class == "net/minecraft/resources/ResourceKey"
        && method == "identifier"
        && ret == "net.minecraft.resources.Identifier"
    {
        return "return horizonIdentifier;".to_owned();
    }
    if class == "net/minecraft/resources/ResourceKey" {
        if method == "registry" && ret == "net.minecraft.resources.Identifier" {
            return "return horizonRegistryName;".to_owned();
        }
        if method == "location" && ret == "net.minecraft.resources.Identifier" {
            return "return horizonIdentifier;".to_owned();
        }
        if method == "registryKey" && ret == "net.minecraft.resources.ResourceKey" {
            return "return this;".to_owned();
        }
        if method == "isFor" && ret == "boolean" {
            return "return p0 instanceof ResourceKey other && horizonRegistryName.equals(other.horizonRegistryName);".to_owned();
        }
        if method == "toString" && ret == "java.lang.String" {
            return "return horizonRegistryName + \" / \" + horizonIdentifier;".to_owned();
        }
        if method == "hashCode" && ret == "int" {
            return "return java.util.Objects.hash(horizonRegistryName, horizonIdentifier);"
                .to_owned();
        }
        if method == "equals" && ret == "boolean" {
            return "return p0 instanceof ResourceKey other && horizonRegistryName.equals(other.horizonRegistryName) && horizonIdentifier.equals(other.horizonIdentifier);".to_owned();
        }
    }
    if class == "net/minecraft/world/entity/ai/attributes/RangedAttribute"
        && method == "setSyncable"
    {
        return "return this;".to_owned();
    }
    if class == "net/minecraft/world/effect/MobEffect"
        && matches!(method, "addAttributeModifier" | "setBlendDuration")
    {
        return "return this;".to_owned();
    }
    if class == "net/minecraft/world/level/block/state/BlockBehaviour$Properties"
        && ret == "net.minecraft.world.level.block.state.BlockBehaviour.Properties"
    {
        match (method, descriptor) {
            ("of", _) => return "return new Properties();".to_owned(),
            (
                "strength",
                "(F)Lnet/minecraft/world/level/block/state/BlockBehaviour$Properties;",
            ) => {
                return "horizonDestroyTime = Math.max(0.0f, p0); horizonExplosionResistance = Math.max(0.0f, p0); return this;".to_owned();
            }
            (
                "strength",
                "(FF)Lnet/minecraft/world/level/block/state/BlockBehaviour$Properties;",
            ) => {
                return "horizonDestroyTime = Math.max(0.0f, p0); horizonExplosionResistance = Math.max(0.0f, p1); return this;".to_owned();
            }
            ("noOcclusion", _) => return "horizonNoOcclusion = true; return this;".to_owned(),
            ("noLootTable", _) => return "horizonNoLootTable = true; return this;".to_owned(),
            ("requiresCorrectToolForDrops", _) => {
                return "horizonCorrectTool = true; return this;".to_owned();
            }
            ("randomTicks", _) => return "horizonRandomTicks = true; return this;".to_owned(),
            ("friction", _) => {
                return "horizonFriction = Math.max(0.0f, p0); return this;".to_owned();
            }
            ("speedFactor", _) => {
                return "horizonSpeedFactor = Math.max(0.0f, p0); return this;".to_owned();
            }
            ("jumpFactor", _) => {
                return "horizonJumpFactor = Math.max(0.0f, p0); return this;".to_owned();
            }
            _ => {}
        }
        return "return this;".to_owned();
    }
    if class == "net/minecraft/world/level/block/state/StateDefinition" && method == "any" {
        return "return new net.minecraft.world.level.block.state.BlockState();".to_owned();
    }
    if class == "net/minecraft/world/level/block/Block" {
        if method == "asItem" && ret == "net.minecraft.world.item.Item" {
            return "return new net.minecraft.world.item.Item();".to_owned();
        }
        if method == "getStateDefinition"
            && ret == "net.minecraft.world.level.block.state.StateDefinition"
        {
            return "return stateDefinition;".to_owned();
        }
        if method == "defaultBlockState"
            && ret == "net.minecraft.world.level.block.state.BlockState"
        {
            return "return new net.minecraft.world.level.block.state.BlockState();".to_owned();
        }
    }
    if class == "net/minecraft/world/item/Item$Properties"
        && ret == "net.minecraft.world.item.Item.Properties"
    {
        match (method, descriptor) {
            ("stacksTo", "(I)Lnet/minecraft/world/item/Item$Properties;") => {
                return "horizonMaxStackSize = Math.max(1, Math.min(99, p0)); return this;"
                    .to_owned();
            }
            ("durability", "(I)Lnet/minecraft/world/item/Item$Properties;") => {
                return "horizonMaxDamage = Math.max(0, p0); horizonMaxStackSize = 1; return this;"
                    .to_owned();
            }
            (
                "craftRemainder",
                "(Lnet/minecraft/world/item/Item;)Lnet/minecraft/world/item/Item$Properties;",
            ) => {
                return "horizonCraftRemainder = p0; return this;".to_owned();
            }
            ("fireResistant", "()Lnet/minecraft/world/item/Item$Properties;") => {
                return "horizonFireResistant = true; return this;".to_owned();
            }
            _ => return "return this;".to_owned(),
        }
    }
    if class == "net/minecraft/world/item/Item" {
        match (method, ret, descriptor) {
            ("getMaxStackSize", "int", "()I") => return "return horizonMaxStackSize;".to_owned(),
            ("getMaxDamage", "int", "()I") => return "return horizonMaxDamage;".to_owned(),
            ("isDamageable", "boolean", "()Z") => return "return horizonMaxDamage > 0;".to_owned(),
            (
                "getCraftingRemainingItem",
                "net.minecraft.world.item.Item",
                "()Lnet/minecraft/world/item/Item;",
            ) => return "return horizonCraftRemainder;".to_owned(),
            ("isFireResistant", "boolean", "()Z") => {
                return "return horizonFireResistant;".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/world/item/Item"
        && method == "getDefaultInstance"
        && ret == "net.minecraft.world.item.ItemStack"
        && descriptor == "()Lnet/minecraft/world/item/ItemStack;"
    {
        return "return new net.minecraft.world.item.ItemStack(this, 1);".to_owned();
    }
    if class == "net/minecraft/world/item/ItemStack" {
        match (method, ret) {
            ("getCount", "int") => return "return horizonCount;".to_owned(),
            ("setCount", "void") => {
                return "this.horizonCount = p0;".to_owned();
            }
            ("isEmpty", "boolean") => {
                return "return horizonCount <= 0;".to_owned();
            }
            ("getTags", "java.util.stream.Stream")
                if descriptor == "()Ljava/util/stream/Stream;" =>
            {
                return "return java.util.stream.Stream.empty();".to_owned();
            }
            ("getMaxStackSize", "int") => {
                return "return horizonItem == null ? 64 : horizonItem.getMaxStackSize();"
                    .to_owned();
            }
            ("hasFoil", "boolean") => return "return false;".to_owned(),
            ("isDamaged", "boolean") => return "return horizonDamage > 0;".to_owned(),
            ("getDamageValue", "int") => return "return horizonDamage;".to_owned(),
            ("setDamageValue", "void") => return "horizonDamage = Math.max(0, p0);".to_owned(),
            ("getMaxDamage", "int") => {
                return "return horizonItem == null ? 0 : horizonItem.getMaxDamage();".to_owned();
            }
            ("isDamageable", "boolean") => {
                return "return horizonItem != null && horizonItem.isDamageable();".to_owned();
            }
            ("isEnchanted", "boolean") => return "return false;".to_owned(),
            ("getUseDuration", "int") => return "return 0;".to_owned(),
            ("getUseAnimation", "net.minecraft.world.item.UseAnim") => {
                return "return net.minecraft.world.item.UseAnim.NONE;".to_owned();
            }
            ("getHoverName", "net.minecraft.network.chat.Component") => {
                return "return net.minecraft.network.chat.Component.literal(horizonItem.toString());".to_owned();
            }
            ("getItem", "net.minecraft.world.item.Item") => {
                return "return horizonItem;".to_owned();
            }
            ("copy", "net.minecraft.world.item.ItemStack")
            | ("copyWithCount", "net.minecraft.world.item.ItemStack") => {
                let count = if method == "copyWithCount" {
                    "p0"
                } else {
                    "horizonCount"
                };
                return format!(
                    "net.minecraft.world.item.ItemStack value = new net.minecraft.world.item.ItemStack(horizonItem, {count}); value.horizonDamage = horizonDamage; value.horizonComponents.putAll(horizonComponents); return value;"
                );
            }
            ("grow", "void") => {
                return "this.horizonCount += p0;".to_owned();
            }
            ("shrink", "void") => {
                return "this.horizonCount -= p0;".to_owned();
            }
            ("is", "boolean") if descriptor == "(Lnet/minecraft/world/item/Item;)Z" => {
                return "return horizonItem == p0;".to_owned();
            }
            ("is", "boolean") if descriptor == "(Lnet/minecraft/tags/TagKey;)Z" => {
                return "return false;".to_owned();
            }
            ("isSameItem", "boolean")
                if descriptor == "(Lnet/minecraft/world/item/ItemStack;)Z" =>
            {
                return "return p0 != null && horizonItem == p0.horizonItem;".to_owned();
            }
            ("isSameItemSameTags", "boolean")
                if descriptor == "(Lnet/minecraft/world/item/ItemStack;)Z" =>
            {
                return "return p0 != null && horizonItem == p0.horizonItem && horizonComponents.equals(p0.horizonComponents);".to_owned();
            }
            // Data components are keyed by their identity-bearing type.  The
            // façade keeps the common typed operations after erasure; Java's
            // call site inserts the cast required by the generic API.
            ("get", "java.lang.Object")
                if descriptor
                    == "(Lnet/minecraft/core/component/DataComponentType;)Ljava/lang/Object;" =>
            {
                return "return horizonComponents.get(p0);".to_owned();
            }
            ("getOrDefault", "java.lang.Object")
                if descriptor
                    == "(Lnet/minecraft/core/component/DataComponentType;Ljava/lang/Object;)Ljava/lang/Object;" =>
            {
                return "Object value = horizonComponents.get(p0); return value == null ? p1 : value;"
                    .to_owned();
            }
            ("set", "java.lang.Object")
                if descriptor
                    == "(Lnet/minecraft/core/component/DataComponentType;Ljava/lang/Object;)Ljava/lang/Object;" =>
            {
                return "return horizonComponents.put(p0, p1);".to_owned();
            }
            ("remove", "java.lang.Object")
                if descriptor
                    == "(Lnet/minecraft/core/component/DataComponentType;)Ljava/lang/Object;" =>
            {
                return "return horizonComponents.remove(p0);".to_owned();
            }
            ("has", "boolean") | ("hasNonDefault", "boolean")
                if descriptor == "(Lnet/minecraft/core/component/DataComponentType;)Z" =>
            {
                return "return horizonComponents.containsKey(p0);".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/nbt/CompoundTag" {
        match method {
            "putString" if descriptor == "(Ljava/lang/String;Ljava/lang/String;)V" => {
                return "horizonValues.put(p0, p1);".to_owned();
            }
            "putInt" if descriptor == "(Ljava/lang/String;I)V" => {
                return "horizonValues.put(p0, p1);".to_owned();
            }
            "putBoolean" if descriptor == "(Ljava/lang/String;Z)V" => {
                return "horizonValues.put(p0, p1);".to_owned();
            }
            "putLong" if descriptor == "(Ljava/lang/String;J)V" => {
                return "horizonValues.put(p0, p1);".to_owned();
            }
            _ if matches!(method, "putByte" | "putShort" | "putFloat" | "putDouble")
                && matches!(
                    descriptor,
                    "(Ljava/lang/String;B)V"
                        | "(Ljava/lang/String;S)V"
                        | "(Ljava/lang/String;F)V"
                        | "(Ljava/lang/String;D)V"
                ) =>
            {
                return "horizonValues.put(p0, p1);".to_owned();
            }
            "putByteArray" if descriptor == "(Ljava/lang/String;[B)V" => {
                return "horizonValues.put(p0, p1 == null ? new byte[0] : p1.clone());".to_owned();
            }
            "put"
                if descriptor
                    == "(Ljava/lang/String;Lnet/minecraft/nbt/Tag;)Lnet/minecraft/nbt/Tag;" =>
            {
                return "Object previous = horizonValues.put(p0, p1); return previous instanceof net.minecraft.nbt.Tag ? (net.minecraft.nbt.Tag) previous : null;".to_owned();
            }
            "getString" if descriptor == "(Ljava/lang/String;)Ljava/lang/String;" => {
                return "return horizonValues.get(p0) instanceof String ? (String) horizonValues.get(p0) : \"\";".to_owned();
            }
            "getInt" if descriptor == "(Ljava/lang/String;)I" => {
                return "return horizonValues.get(p0) instanceof Integer ? ((Integer) horizonValues.get(p0)).intValue() : 0;".to_owned();
            }
            "getBoolean" if descriptor == "(Ljava/lang/String;)Z" => {
                return "return horizonValues.get(p0) instanceof Boolean && ((Boolean) horizonValues.get(p0)).booleanValue();".to_owned();
            }
            "getLong" if descriptor == "(Ljava/lang/String;)J" => {
                return "return horizonValues.get(p0) instanceof Long ? ((Long) horizonValues.get(p0)).longValue() : 0L;".to_owned();
            }
            "getByte" if descriptor == "(Ljava/lang/String;)B" => {
                return "return horizonValues.get(p0) instanceof Number ? ((Number) horizonValues.get(p0)).byteValue() : 0;".to_owned();
            }
            "getShort" if descriptor == "(Ljava/lang/String;)S" => {
                return "return horizonValues.get(p0) instanceof Number ? ((Number) horizonValues.get(p0)).shortValue() : 0;".to_owned();
            }
            "getFloat" if descriptor == "(Ljava/lang/String;)F" => {
                return "return horizonValues.get(p0) instanceof Number ? ((Number) horizonValues.get(p0)).floatValue() : 0.0f;".to_owned();
            }
            "getDouble" if descriptor == "(Ljava/lang/String;)D" => {
                return "return horizonValues.get(p0) instanceof Number ? ((Number) horizonValues.get(p0)).doubleValue() : 0.0d;".to_owned();
            }
            "getByteArray" if descriptor == "(Ljava/lang/String;)[B" => {
                return "Object value = horizonValues.get(p0); return value instanceof byte[] ? ((byte[]) value).clone() : new byte[0];".to_owned();
            }
            "get" if descriptor == "(Ljava/lang/String;)Lnet/minecraft/nbt/Tag;" => {
                return "Object value = horizonValues.get(p0); return value instanceof net.minecraft.nbt.Tag ? (net.minecraft.nbt.Tag) value : null;".to_owned();
            }
            "getCompound"
                if descriptor == "(Ljava/lang/String;)Lnet/minecraft/nbt/CompoundTag;" =>
            {
                return "Object value = horizonValues.get(p0); return value instanceof net.minecraft.nbt.CompoundTag ? (net.minecraft.nbt.CompoundTag) value : new net.minecraft.nbt.CompoundTag();".to_owned();
            }
            "getList" if descriptor == "(Ljava/lang/String;I)Lnet/minecraft/nbt/ListTag;" => {
                return "Object value = horizonValues.get(p0); return value instanceof net.minecraft.nbt.ListTag ? (net.minecraft.nbt.ListTag) value : new net.minecraft.nbt.ListTag();".to_owned();
            }
            "contains" if descriptor == "(Ljava/lang/String;)Z" => {
                return "return horizonValues.containsKey(p0);".to_owned();
            }
            "contains" if descriptor == "(Ljava/lang/String;I)Z" => {
                return "return horizonValues.containsKey(p0);".to_owned();
            }
            "isEmpty" if descriptor == "()Z" => {
                return "return horizonValues.isEmpty();".to_owned();
            }
            "size" if descriptor == "()I" => {
                return "return horizonValues.size();".to_owned();
            }
            "getAllKeys" if descriptor == "()Ljava/util/Set;" => {
                return "return horizonValues.keySet();".to_owned();
            }
            "remove" if descriptor == "(Ljava/lang/String;)Lnet/minecraft/nbt/Tag;" => {
                return "Object previous = horizonValues.remove(p0); return previous instanceof net.minecraft.nbt.Tag ? (net.minecraft.nbt.Tag) previous : null;".to_owned();
            }
            "copy" if descriptor == "()Lnet/minecraft/nbt/CompoundTag;" => {
                return "net.minecraft.nbt.CompoundTag value = new net.minecraft.nbt.CompoundTag(); value.horizonValues.putAll(horizonValues); return value;".to_owned();
            }
            "merge"
                if descriptor
                    == "(Lnet/minecraft/nbt/CompoundTag;)Lnet/minecraft/nbt/CompoundTag;" =>
            {
                return "if (p0 != null) horizonValues.putAll(p0.horizonValues); return this;"
                    .to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/nbt/ListTag" {
        match method {
            "add" if descriptor == "(Lnet/minecraft/nbt/Tag;)Z" => return "return horizonValues.add(p0);".to_owned(),
            "add" if descriptor == "(ILnet/minecraft/nbt/Tag;)V" => return "horizonValues.add(p0, p1);".to_owned(),
            "get" if descriptor == "(I)Lnet/minecraft/nbt/Tag;" => return "return horizonValues.get(p0);".to_owned(),
            "remove" if descriptor == "(I)Lnet/minecraft/nbt/Tag;" => return "return horizonValues.remove(p0);".to_owned(),
            "size" if descriptor == "()I" => return "return horizonValues.size();".to_owned(),
            "isEmpty" if descriptor == "()Z" => return "return horizonValues.isEmpty();".to_owned(),
            "getCompound" if descriptor == "(I)Lnet/minecraft/nbt/CompoundTag;" => return "Object value = horizonValues.get(p0); return value instanceof net.minecraft.nbt.CompoundTag ? (net.minecraft.nbt.CompoundTag) value : new net.minecraft.nbt.CompoundTag();".to_owned(),
            "getString" if descriptor == "(I)Ljava/lang/String;" => return "Object value = horizonValues.get(p0); return value instanceof String ? (String) value : value == null ? \"\" : value.toString();".to_owned(),
            _ => {}
        }
    }
    if class == "net/minecraft/world/item/CreativeModeTab$Builder" {
        if ret == "net.minecraft.world.item.CreativeModeTab.Builder" {
            return "return this;".to_owned();
        }
        if method == "build" && ret == "net.minecraft.world.item.CreativeModeTab" {
            return "return new net.minecraft.world.item.CreativeModeTab();".to_owned();
        }
    }
    if class == "net/minecraft/world/item/CreativeModeTab"
        && method == "builder"
        && ret == "net.minecraft.world.item.CreativeModeTab.Builder"
    {
        return "return new CreativeModeTab.Builder();".to_owned();
    }
    if method == "create"
        && matches!(
            class,
            "net/minecraft/world/level/block/state/properties/BooleanProperty"
                | "net/minecraft/world/level/block/state/properties/IntegerProperty"
                | "net/minecraft/world/level/block/state/properties/EnumProperty"
                | "net/minecraft/world/level/block/state/properties/DirectionProperty"
        )
    {
        let simple = class.rsplit('/').next().unwrap_or(class);
        let expected = class.replace('/', ".");
        if ret == expected {
            return format!("return new {simple}();");
        }
    }
    if class == "net/minecraft/sounds/SoundEvent"
        && matches!(method, "createVariableRangeEvent" | "createFixedRangeEvent")
        && ret == "net.minecraft.sounds.SoundEvent"
    {
        return "return new SoundEvent();".to_owned();
    }
    if class == "net/minecraft/network/chat/Component"
        && matches!(
            method,
            "translatable"
                | "literal"
                | "empty"
                | "translatableEscape"
                | "translatableWithFallback"
        )
        && ret == "net.minecraft.network.chat.MutableComponent"
    {
        if method == "literal" {
            return "net.minecraft.network.chat.MutableComponent value = new net.minecraft.network.chat.MutableComponent(); value.horizonText = p0; return value;".to_owned();
        }
        if method.starts_with("translatable") {
            return "net.minecraft.network.chat.MutableComponent value = new net.minecraft.network.chat.MutableComponent(); value.horizonKey = p0; value.horizonText = p0; return value;".to_owned();
        }
        return "return new net.minecraft.network.chat.MutableComponent();".to_owned();
    }
    if class == "net/minecraft/network/chat/MutableComponent" {
        if method == "getString" && ret == "java.lang.String" {
            return "return horizonText;".to_owned();
        }
        if method == "append"
            && ret == "net.minecraft.network.chat.MutableComponent"
            && descriptor.contains("Lnet/minecraft/network/chat/Component;")
        {
            return "if (p0 != null) horizonText += p0.getString(); return this;".to_owned();
        }
        if method == "append"
            && ret == "net.minecraft.network.chat.MutableComponent"
            && descriptor.contains("Ljava/lang/String;")
        {
            return "if (p0 != null) horizonText += p0; return this;".to_owned();
        }
        if method == "copy"
            && matches!(
                ret,
                "net.minecraft.network.chat.MutableComponent"
                    | "net.minecraft.network.chat.Component"
            )
        {
            return "net.minecraft.network.chat.MutableComponent value = new net.minecraft.network.chat.MutableComponent(); value.horizonText = horizonText; value.horizonKey = horizonKey; return value;".to_owned();
        }
        if method == "plainCopy" && ret == "net.minecraft.network.chat.MutableComponent" {
            return "net.minecraft.network.chat.MutableComponent value = new net.minecraft.network.chat.MutableComponent(); value.horizonText = horizonText; return value;".to_owned();
        }
        if method == "withStyle" && ret == "net.minecraft.network.chat.MutableComponent" {
            return "return this;".to_owned();
        }
    }
    if class == "net/minecraft/world/entity/EntityType$Builder" {
        if method == "sized"
            && ret == "net.minecraft.world.entity.EntityType.Builder"
            && descriptor.contains("Lnet/minecraft/world/entity/EntityDimensions;")
        {
            // The value overload: carry the triple's width and height into the
            // same fields the two-float overload sets.
            return "if (p0 != null) { horizonWidth = p0.getWidth(); horizonHeight = p0.getHeight(); } return this;".to_owned();
        }
        if method == "sized" && ret == "net.minecraft.world.entity.EntityType.Builder" {
            return "horizonWidth = p0; horizonHeight = p1; return this;".to_owned();
        }
        if method == "clientTrackingRange" && ret == "net.minecraft.world.entity.EntityType.Builder"
        {
            return "horizonTrackingRange = p0; return this;".to_owned();
        }
        if method == "updateInterval" && ret == "net.minecraft.world.entity.EntityType.Builder" {
            return "horizonUpdateInterval = p0; return this;".to_owned();
        }
        if method == "fireImmune" && ret == "net.minecraft.world.entity.EntityType.Builder" {
            return "horizonFireImmune = true; return this;".to_owned();
        }
        if method == "noSummon" && ret == "net.minecraft.world.entity.EntityType.Builder" {
            return "horizonCanSummon = false; return this;".to_owned();
        }
        if method == "noSave" && ret == "net.minecraft.world.entity.EntityType.Builder" {
            return "horizonSaves = false; return this;".to_owned();
        }
        if method == "canSpawnFarFromPlayer"
            && ret == "net.minecraft.world.entity.EntityType.Builder"
        {
            return "horizonCanSpawnFarFromPlayer = true; return this;".to_owned();
        }
        if matches!(method, "immuneTo" | "requiredFeatures")
            && ret == "net.minecraft.world.entity.EntityType.Builder"
        {
            // Bounded: the extra block tag / feature set is not recorded yet,
            // so the chain stays alive without pretending to filter spawns.
            return "return this;".to_owned();
        }
        if method == "of" && ret == "net.minecraft.world.entity.EntityType.Builder" {
            if descriptor.contains("Lnet/minecraft/world/entity/MobCategory;") {
                return "EntityType.Builder builder = new EntityType.Builder(); builder.horizonCategory = p1; return builder;".to_owned();
            }
            return "return new EntityType.Builder();".to_owned();
        }
        if ret == "net.minecraft.world.entity.EntityType.Builder" {
            return "return this;".to_owned();
        }
        if method == "build" && ret == "net.minecraft.world.entity.EntityType" {
            // Both the `String` and the `ResourceKey` overload name a type; the
            // bounded value carries the flags the builder set, so a mod reading
            // `canSummon()`/`canSpawnFarFromPlayer()` after `build` sees them.
            return "net.minecraft.world.entity.EntityType value = new net.minecraft.world.entity.EntityType(horizonWidth, horizonHeight, horizonTrackingRange, horizonUpdateInterval, horizonFireImmune, horizonCategory); value.horizonCanSpawnFarFromPlayer = horizonCanSpawnFarFromPlayer; value.horizonCanSummon = horizonCanSummon; value.horizonSaves = horizonSaves; return value;".to_owned();
        }
    }
    if class == "net/minecraft/world/entity/EntityType" {
        match (method, ret) {
            ("getWidth", "float") => return "return horizonWidth;".to_owned(),
            ("getHeight", "float") => return "return horizonHeight;".to_owned(),
            ("getTrackingRange", "int") => return "return horizonTrackingRange;".to_owned(),
            ("getUpdateInterval", "int") => return "return horizonUpdateInterval;".to_owned(),
            ("fireImmune", "boolean") => return "return horizonFireImmune;".to_owned(),
            ("canSpawnFarFromPlayer", "boolean") => {
                return "return horizonCanSpawnFarFromPlayer;".to_owned();
            }
            ("canSummon", "boolean") => return "return horizonCanSummon;".to_owned(),
            ("getDescriptionId", "java.lang.String") => {
                return "return horizonDescriptionId;".to_owned();
            }
            ("getDimensions", "net.minecraft.world.entity.EntityDimensions") => {
                return "return net.minecraft.world.entity.EntityDimensions.scalable(horizonWidth, horizonHeight);".to_owned();
            }
            ("getDescription", "net.minecraft.network.chat.Component") => {
                return "return net.minecraft.network.chat.Component.literal(horizonDescriptionId);"
                    .to_owned();
            }
            // 26.3 keys entity types through `BuiltInRegistries.ENTITY_TYPE`;
            // these two are bounded placeholders for a snapshot whose mapping
            // still named them on the type itself.
            ("getDefaultLootTable", "java.lang.String") => {
                return "return \"minecraft:empty\";".to_owned();
            }
            ("getDefaultLootTable", "net.minecraft.resources.ResourceKey") => {
                return "return new net.minecraft.resources.ResourceKey();".to_owned();
            }
            ("getRegistryName", "net.minecraft.resources.Identifier") => {
                return "return new net.minecraft.resources.Identifier(\"minecraft\", \"unknown\");"
                    .to_owned();
            }
            _ => {}
        }
        if method == "isEnabled"
            && ret == "boolean"
            && descriptor.contains("Lnet/minecraft/world/flag/FeatureFlagSet;")
        {
            // Feature flags gate vanilla content, not a mod's own registration.
            return "return true;".to_owned();
        }
    }
    if class == "net/minecraft/world/entity/EntityType"
        && method == "getCategory"
        && ret == "net.minecraft.world.entity.MobCategory"
    {
        return "return horizonCategory;".to_owned();
    }
    if class == "net/minecraft/world/entity/SpawnPlacements" {
        if method == "register" && ret == "void" {
            // Only write a slot the descriptor actually carries, so a snapshot
            // that adds a height window still compiles.
            let params = descriptor_types(descriptor);
            let index = |ty: &str| params.iter().position(|param| param == ty);
            if let (Some(entity), Some(placement), Some(heightmap)) = (
                index("net/minecraft/world/entity/EntityType"),
                index("net/minecraft/world/entity/SpawnPlacementType"),
                index("net/minecraft/world/level/levelgen/Heightmap$Types"),
            ) {
                return format!(
                    "if (p{entity} != null) {{ horizonPlacements.put(p{entity}, p{placement}); horizonHeightmaps.put(p{entity}, p{heightmap}); horizonSpawnRules.put(p{entity}, Boolean.TRUE); }}"
                );
            }
            return "if (p0 != null) { horizonSpawnRules.put(p0, Boolean.TRUE); }".to_owned();
        }
        if method == "getPlacementType" && ret == "net.minecraft.world.entity.SpawnPlacementType" {
            return "net.minecraft.world.entity.SpawnPlacementType value = horizonPlacements.get(p0); return value == null ? net.minecraft.world.entity.SpawnPlacementTypes.NO_RESTRICTIONS : value;".to_owned();
        }
        if method == "getHeightmapType"
            && ret == "net.minecraft.world.level.levelgen.Heightmap.Types"
        {
            return "net.minecraft.world.level.levelgen.Heightmap.Types value = horizonHeightmaps.get(p0); return value == null ? net.minecraft.world.level.levelgen.Heightmap.Types.MOTION_BLOCKING_NO_LEAVES : value;".to_owned();
        }
        if method == "isSpawnPositionOk"
            && ret == "boolean"
            && descriptor.contains("LevelReader")
            && descriptor.contains("BlockPos")
        {
            return "net.minecraft.world.entity.SpawnPlacementType value = horizonPlacements.get(p0); return value == null || value.isSpawnPositionOk(p1, p2, p0);".to_owned();
        }
        if method == "checkSpawnRules" && ret == "boolean" {
            return "return horizonSpawnRules.getOrDefault(p0, Boolean.TRUE);".to_owned();
        }
        if method == "checkSpawnObstruction" && ret == "boolean" {
            // Obstruction is a world-shape question; a shape-only façade
            // answers "not obstructed" so a mod's own checks still run.
            return "return true;".to_owned();
        }
    }
    if class == "net/minecraft/world/entity/MobCategory" {
        match (method, ret) {
            ("getName", "java.lang.String") => {
                return "if (name().equals(\"MONSTER\")) return \"monster\"; if (name().equals(\"CREATURE\")) return \"creature\"; if (name().equals(\"AMBIENT\")) return \"ambient\"; if (name().equals(\"AXOLOTLS\")) return \"axolotls\"; if (name().equals(\"UNDERGROUND_WATER_CREATURE\")) return \"underground_water_creature\"; if (name().equals(\"WATER_CREATURE\")) return \"water_creature\"; if (name().equals(\"WATER_AMBIENT\")) return \"water_ambient\"; return \"misc\";".to_owned();
            }
            ("getMaxInstancesPerChunk", "int") => {
                return "if (name().equals(\"MONSTER\")) return 70; if (name().equals(\"CREATURE\")) return 10; if (name().equals(\"AMBIENT\")) return 15; if (name().equals(\"WATER_AMBIENT\")) return 20; if (name().equals(\"MISC\")) return -1; return 5;".to_owned();
            }
            ("isFriendly", "boolean") => {
                return "return !name().equals(\"MONSTER\");".to_owned();
            }
            ("isPersistent", "boolean") => {
                return "return name().equals(\"CREATURE\") || name().equals(\"MISC\");".to_owned();
            }
            ("getDespawnDistance", "int") => {
                return "return name().equals(\"WATER_AMBIENT\") ? 64 : 128;".to_owned();
            }
            ("getNoDespawnDistance", "int") => {
                // Vanilla's `MobCategory.MAX_NO_DESPAWN_DISTANCE`: a mob inside
                // this block radius of a player is never removed by distance.
                return "return 32;".to_owned();
            }
            ("byName", "net.minecraft.world.entity.MobCategory") => {
                // Vanilla answers `null` for an unknown name; the bounded value
                // falls back to MISC so a mod's lookup never NPEs.
                return "if (p0 == null) return net.minecraft.world.entity.MobCategory.MISC; if (p0.equals(\"monster\")) return net.minecraft.world.entity.MobCategory.MONSTER; if (p0.equals(\"creature\")) return net.minecraft.world.entity.MobCategory.CREATURE; if (p0.equals(\"ambient\")) return net.minecraft.world.entity.MobCategory.AMBIENT; if (p0.equals(\"axolotls\")) return net.minecraft.world.entity.MobCategory.AXOLOTLS; if (p0.equals(\"underground_water_creature\")) return net.minecraft.world.entity.MobCategory.UNDERGROUND_WATER_CREATURE; if (p0.equals(\"water_creature\")) return net.minecraft.world.entity.MobCategory.WATER_CREATURE; if (p0.equals(\"water_ambient\")) return net.minecraft.world.entity.MobCategory.WATER_AMBIENT; return net.minecraft.world.entity.MobCategory.MISC;".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/client/KeyMapping$Category"
        && method == "register"
        && ret == "net.minecraft.client.KeyMapping.Category"
    {
        return "return new Category(p0);".to_owned();
    }
    if class == "net/minecraft/client/Minecraft"
        && method == "getInstance"
        && ret == "net.minecraft.client.Minecraft"
    {
        return "return new Minecraft();".to_owned();
    }
    if class == "net/minecraft/client/KeyMapping"
        && method == "getName"
        && ret == "java.lang.String"
    {
        return "return horizonName;".to_owned();
    }
    if class == "net/minecraft/world/item/CreativeModeTab$Builder" {
        if ret == "net.minecraft.world.item.CreativeModeTab.Builder" {
            return "return this;".to_owned();
        }
        if method == "build" && ret == "net.minecraft.world.item.CreativeModeTab" {
            return "return new net.minecraft.world.item.CreativeModeTab();".to_owned();
        }
    }
    if class == "net/minecraft/world/level/block/state/BlockState" {
        match method {
            "getFluidState" if ret == "net.minecraft.world.level.material.FluidState" => {
                return "return new net.minecraft.world.level.material.FluidState();".to_owned();
            }
            "getBlock" if ret == "net.minecraft.world.level.block.Block" => {
                return "return horizonBlock;".to_owned();
            }
            "is" if ret == "boolean"
                && descriptor == "(Lnet/minecraft/world/level/block/Block;)Z" =>
            {
                return "return horizonBlock == p0;".to_owned();
            }
            "is" if ret == "boolean" && descriptor == "(Lnet/minecraft/tags/TagKey;)Z" => {
                return "return false;".to_owned();
            }
            "hasBlockEntity" if ret == "boolean" && descriptor == "()Z" => {
                return "return false;".to_owned();
            }
            "isRandomlyTicking" if ret == "boolean" && descriptor == "()Z" => {
                return "return false;".to_owned();
            }
            "isAir" if ret == "boolean" => return "return false;".to_owned(),
            "canOcclude" if ret == "boolean" => return "return true;".to_owned(),
            "isSolidRender" if ret == "boolean" => return "return true;".to_owned(),
            "getLightEmission" if ret == "int" => return "return 0;".to_owned(),
            "setValue" if ret == "net.minecraft.world.level.block.state.BlockState" => {
                return "horizonValues.put(p0, p1); return this;".to_owned();
            }
            "getValue" if ret == "java.lang.Comparable" => {
                return "return (java.lang.Comparable) horizonValues.get(p0);".to_owned();
            }
            "getValue" if ret == "java.lang.Object" => {
                return "return horizonValues.get(p0);".to_owned();
            }
            "hasProperty" if ret == "boolean" => {
                return "return horizonValues.containsKey(p0);".to_owned();
            }
            "getProperties"
                if matches!(
                    ret,
                    "java.util.Collection" | "java.util.Set" | "java.util.Collection<?>"
                ) =>
            {
                return "return horizonValues.keySet();".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/world/level/material/FluidState" {
        match (method, ret) {
            ("isEmpty", "boolean") => return "return horizonEmpty;".to_owned(),
            ("isSource", "boolean") => {
                return "return !horizonEmpty && horizonAmount >= 8;".to_owned();
            }
            ("getAmount", "int") => return "return horizonAmount;".to_owned(),
            ("getHeight", "float") => return "return horizonAmount / 9.0f;".to_owned(),
            _ => {}
        }
    }
    if matches!(
        class,
        "net/minecraft/world/level/Level" | "net/minecraft/server/level/ServerLevel"
    ) {
        match (method, ret, descriptor) {
            ("getGameTime", "long", "()J") | ("getDayTime", "long", "()J") => {
                return "return horizonGameTime;".to_owned();
            }
            ("isLoaded", "boolean", "(Lnet/minecraft/core/BlockPos;)Z")
            | ("hasChunkAt", "boolean", "(Lnet/minecraft/core/BlockPos;)Z") => {
                return "return horizonBlocks.containsKey(horizonBlockKey(p0)) || horizonBlockEntities.containsKey(horizonBlockKey(p0));".to_owned();
            }
            ("canSeeSky", "boolean", "(Lnet/minecraft/core/BlockPos;)Z") => {
                return "return true;".to_owned();
            }
            ("getSkyDarken", "int", "()I") => {
                return "return 0;".to_owned();
            }
            ("isRaining", "boolean", "()Z") => {
                return "return horizonRainLevel > 0.0f;".to_owned();
            }
            ("isThundering", "boolean", "()Z") => {
                return "return horizonThunderLevel > 0.0f;".to_owned();
            }
            ("isRainingAt", "boolean", "(Lnet/minecraft/core/BlockPos;)Z") => {
                return "return horizonRainLevel > 0.0f && canSeeSky(p0);".to_owned();
            }
            ("getRainLevel", "float", "(F)F") => {
                return "return horizonRainLevel;".to_owned();
            }
            ("getThunderLevel", "float", "(F)F") => {
                return "return horizonThunderLevel;".to_owned();
            }
            (
                "dimension",
                "net.minecraft.resources.ResourceKey",
                "()Lnet/minecraft/resources/ResourceKey;",
            ) => {
                return "return horizonDimension;".to_owned();
            }
            (
                "getBlockState",
                "net.minecraft.world.level.block.state.BlockState",
                "(Lnet/minecraft/core/BlockPos;)Lnet/minecraft/world/level/block/state/BlockState;",
            ) => {
                return "return horizonBlocks.getOrDefault(horizonBlockKey(p0), new net.minecraft.world.level.block.state.BlockState());".to_owned();
            }
            (
                "getBlockEntity",
                "net.minecraft.world.level.block.entity.BlockEntity",
                "(Lnet/minecraft/core/BlockPos;)Lnet/minecraft/world/level/block/entity/BlockEntity;",
            ) => {
                return "return horizonBlockEntities.get(horizonBlockKey(p0));".to_owned();
            }
            ("setBlock", "boolean", descriptor)
                if descriptor.starts_with("(Lnet/minecraft/core/BlockPos;Lnet/minecraft/world/level/block/state/BlockState;") =>
            {
                return "horizonBlocks.put(horizonBlockKey(p0), p1); return true;".to_owned();
            }
            ("setBlockAndUpdate", "boolean", "(Lnet/minecraft/core/BlockPos;Lnet/minecraft/world/level/block/state/BlockState;)Z") => {
                return "horizonBlocks.put(horizonBlockKey(p0), p1); return true;".to_owned();
            }
            ("setBlockEntity", "void", "(Lnet/minecraft/world/level/block/entity/BlockEntity;)V") => {
                return "if (p0 != null) horizonBlockEntities.put(horizonBlockKey(p0.getBlockPos()), p0);".to_owned();
            }
            ("removeBlock", "boolean", "(Lnet/minecraft/core/BlockPos;Z)Z") => {
                return "horizonBlocks.remove(horizonBlockKey(p0)); horizonBlockEntities.remove(horizonBlockKey(p0)); return true;".to_owned();
            }
            ("isClientSide", "boolean", "()Z") => {
                return "return false;".to_owned();
            }
            ("getMinBuildHeight", "int", "()I") => {
                return "return -64;".to_owned();
            }
            ("getMaxBuildHeight", "int", "()I") => {
                return "return 320;".to_owned();
            }
            ("getSeaLevel", "int", "()I") => {
                return "return 63;".to_owned();
            }
            (
                "getMaxLocalRawBrightness",
                "int",
                "(Lnet/minecraft/core/BlockPos;)I",
            ) => {
                return "return canSeeSky(p0) ? 15 : 0;".to_owned();
            }
            (
                "isOutsideBuildHeight",
                "boolean",
                "(Lnet/minecraft/core/BlockPos;)Z",
            ) => {
                return "return p0.getY() < getMinBuildHeight() || p0.getY() >= getMaxBuildHeight();"
                    .to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/world/entity/Entity" {
        match (method, ret, descriptor) {
            ("getBbWidth", "float", "()F") => return "return horizonBbWidth;".to_owned(),
            ("getBbHeight", "float", "()F") => return "return horizonBbHeight;".to_owned(),
            ("getEyeHeight", "float", "()F") => return "return horizonEyeHeight;".to_owned(),
            ("getEyeY", "double", "()D") => {
                return "return horizonY + horizonEyeHeight;".to_owned();
            }
            (
                "getEyePosition",
                "net.minecraft.world.phys.Vec3",
                "()Lnet/minecraft/world/phys/Vec3;",
            )
            | (
                "getEyePosition",
                "net.minecraft.world.phys.Vec3",
                "(F)Lnet/minecraft/world/phys/Vec3;",
            ) => {
                return "return new net.minecraft.world.phys.Vec3(horizonX, horizonY + horizonEyeHeight, horizonZ);".to_owned();
            }
            ("getBlockX", "int", "()I") => {
                return "return (int) Math.floor(horizonX);".to_owned();
            }
            ("getBlockY", "int", "()I") => {
                return "return (int) Math.floor(horizonY);".to_owned();
            }
            ("getBlockZ", "int", "()I") => {
                return "return (int) Math.floor(horizonZ);".to_owned();
            }
            ("getX", "double", "()D") => return "return horizonX;".to_owned(),
            ("getY", "double", "()D") => return "return horizonY;".to_owned(),
            ("getZ", "double", "()D") => return "return horizonZ;".to_owned(),
            ("setPos", "void", "(DDD)V") => {
                return "horizonX = p0; horizonY = p1; horizonZ = p2;".to_owned();
            }
            ("setPos", "void", "(Lnet/minecraft/world/phys/Vec3;)V") => {
                return "if (p0 != null) { horizonX = p0.x(); horizonY = p0.y(); horizonZ = p0.z(); }"
                    .to_owned();
            }
            ("position", "net.minecraft.world.phys.Vec3", "()Lnet/minecraft/world/phys/Vec3;") => {
                return "return new net.minecraft.world.phys.Vec3(horizonX, horizonY, horizonZ);"
                    .to_owned();
            }
            (
                "getDeltaMovement",
                "net.minecraft.world.phys.Vec3",
                "()Lnet/minecraft/world/phys/Vec3;",
            ) => {
                return "return new net.minecraft.world.phys.Vec3(horizonDeltaX, horizonDeltaY, horizonDeltaZ);"
                    .to_owned();
            }
            ("setDeltaMovement", "void", "(DDD)V") => {
                return "horizonDeltaX = p0; horizonDeltaY = p1; horizonDeltaZ = p2;".to_owned();
            }
            ("setDeltaMovement", "void", "(Lnet/minecraft/world/phys/Vec3;)V") => {
                return "if (p0 != null) { horizonDeltaX = p0.x(); horizonDeltaY = p0.y(); horizonDeltaZ = p0.z(); }"
                    .to_owned();
            }
            ("distanceToSqr", "double", "(Lnet/minecraft/world/entity/Entity;)D") => {
                return "double dx = horizonX - p0.getX(); double dy = horizonY - p0.getY(); double dz = horizonZ - p0.getZ(); return dx * dx + dy * dy + dz * dz;".to_owned();
            }
            ("distanceToSqr", "double", "(DDD)D") => {
                return "double dx = horizonX - p0; double dy = horizonY - p1; double dz = horizonZ - p2; return dx * dx + dy * dy + dz * dz;".to_owned();
            }
            ("distanceTo", "float", "(Lnet/minecraft/world/entity/Entity;)F") => {
                return "return (float) Math.sqrt(distanceToSqr(p0));".to_owned();
            }
            ("blockPosition", "net.minecraft.core.BlockPos", "()Lnet/minecraft/core/BlockPos;") => {
                return "return new net.minecraft.core.BlockPos((int) Math.floor(horizonX), (int) Math.floor(horizonY), (int) Math.floor(horizonZ));".to_owned();
            }
            ("isPassenger", "boolean", "()Z") => return "return false;".to_owned(),
            (
                "getVehicle",
                "net.minecraft.world.entity.Entity",
                "()Lnet/minecraft/world/entity/Entity;",
            ) => return "return null;".to_owned(),
            ("getId", "int", "()I") => return "return horizonEntityId;".to_owned(),
            ("setId", "void", "(I)V") => return "horizonEntityId = p0;".to_owned(),
            (
                "getType",
                "net.minecraft.world.entity.EntityType",
                "()Lnet/minecraft/world/entity/EntityType;",
            ) => return "return horizonType;".to_owned(),
            ("getStringUUID", "java.lang.String", "()Ljava/lang/String;") => {
                return "return horizonUuid.toString();".to_owned();
            }
            ("isOnFire", "boolean", "()Z") => return "return horizonOnFire;".to_owned(),
            ("setSecondsOnFire", "void", "(IZ)V") => return "horizonOnFire = p0 > 0;".to_owned(),
            ("isInWater", "boolean", "()Z") => return "return horizonInWater;".to_owned(),
            ("isInLava", "boolean", "()Z") => return "return horizonInLava;".to_owned(),
            ("isCrouching", "boolean", "()Z") | ("isShiftKeyDown", "boolean", "()Z") => {
                return "return horizonCrouching;".to_owned();
            }
            ("setShiftKeyDown", "void", "(Z)V") => return "horizonCrouching = p0;".to_owned(),
            ("isSprinting", "boolean", "()Z") => return "return horizonSprinting;".to_owned(),
            ("setSprinting", "void", "(Z)V") => return "horizonSprinting = p0;".to_owned(),
            ("level", "net.minecraft.world.level.Level", "()Lnet/minecraft/world/level/Level;") => {
                return "return horizonLevel;".to_owned();
            }
            ("isAlive", "boolean", "()Z") => return "return true;".to_owned(),
            ("getUUID", "java.util.UUID", "()Ljava/util/UUID;") => {
                return "return horizonUuid;".to_owned();
            }
            (
                "getBoundingBox",
                "net.minecraft.world.phys.AABB",
                "()Lnet/minecraft/world/phys/AABB;",
            ) => {
                return "return horizonBoundingBox;".to_owned();
            }
            ("setBoundingBox", "void", "(Lnet/minecraft/world/phys/AABB;)V") => {
                return "horizonBoundingBox = p0;".to_owned();
            }
            ("isOnGround", "boolean", "()Z") | ("onGround", "boolean", "()Z") => {
                return "return horizonOnGround;".to_owned();
            }
            ("setOnGround", "void", "(Z)V") => return "horizonOnGround = p0;".to_owned(),
            ("getXRot", "float", "()F") => return "return horizonXRot;".to_owned(),
            ("setXRot", "void", "(F)V") => return "horizonXRot = p0;".to_owned(),
            ("getYRot", "float", "()F") => return "return horizonYRot;".to_owned(),
            ("setYRot", "void", "(F)V") => return "horizonYRot = p0;".to_owned(),
            ("isRemoved", "boolean", "()Z") => return "return horizonRemoved;".to_owned(),
            ("discard", "void", "()V") => return "horizonRemoved = true;".to_owned(),
            ("remove", "void", "(Lnet/minecraft/world/entity/Entity$RemovalReason;)V") => {
                return "horizonRemoved = true;".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/world/entity/Mob" {
        match (method, ret, descriptor) {
            ("isNoAi", "boolean", "()Z") => return "return horizonNoAi;".to_owned(),
            ("setNoAi", "void", "(Z)V") => return "horizonNoAi = p0;".to_owned(),
            ("isAggressive", "boolean", "()Z") => {
                return "return horizonAggressive;".to_owned();
            }
            ("setAggressive", "void", "(Z)V") => {
                return "horizonAggressive = p0;".to_owned();
            }
            ("isBaby", "boolean", "()Z") => return "return horizonBaby;".to_owned(),
            ("setBaby", "void", "(Z)V") => return "horizonBaby = p0;".to_owned(),
            ("isPersistenceRequired", "boolean", "()Z") => {
                return "return horizonPersistenceRequired;".to_owned();
            }
            ("setPersistenceRequired", "void", "()V") => {
                return "horizonPersistenceRequired = true;".to_owned();
            }
            ("requiresCustomPersistence", "boolean", "()Z") => {
                return "return horizonPersistenceRequired;".to_owned();
            }
            ("canBeLeashed", "boolean", "()Z") => return "return true;".to_owned(),
            (
                "getTarget",
                "net.minecraft.world.entity.LivingEntity",
                "()Lnet/minecraft/world/entity/LivingEntity;",
            ) => return "return horizonTarget;".to_owned(),
            ("setTarget", "void", "(Lnet/minecraft/world/entity/LivingEntity;)V") => {
                return "horizonTarget = p0;".to_owned();
            }
            // Health is declared on `LivingEntity`, but a mod whose receiver is
            // typed `Mob` emits the call with `Mob` as the owner. The fields
            // are inherited by the generated stub, so the bodies are the same.
            ("getHealth", "float", "()F") => return "return horizonHealth;".to_owned(),
            ("getMaxHealth", "float", "()F") => return "return horizonMaxHealth;".to_owned(),
            ("setHealth", "void", "(F)V") => return "horizonHealth = p0;".to_owned(),
            ("heal", "void", "(F)V") => {
                return "horizonHealth = Math.min(horizonMaxHealth, horizonHealth + p0);"
                    .to_owned();
            }
            ("getAttribute", "net.minecraft.world.entity.ai.attributes.AttributeInstance", _) => {
                return "return horizonAttributes.get(p0);".to_owned();
            }
            ("getAttributeValue", "double", _) => return "return 0.0;".to_owned(),
            ("getAttributeBaseValue", "double", _) => return "return 0.0;".to_owned(),
            ("getAttributes", "net.minecraft.world.entity.ai.attributes.AttributeMap", _) => {
                return "return horizonAttributeMap;".to_owned();
            }
            ("getNavigation", _, _) => return "return horizonNavigation;".to_owned(),
            ("goalSelector", _, _) => return "return horizonGoalSelector;".to_owned(),
            ("targetSelector", _, _) => return "return horizonTargetSelector;".to_owned(),
            // `finalizeSpawn` hands the group data back, so a mod's override and
            // its `super` call observe the same marker instead of a throw.
            ("finalizeSpawn", "net.minecraft.world.entity.SpawnGroupData", _) => {
                return "horizonSpawnGroupData = p3; return p3;".to_owned();
            }
            ("convertTo", _, _) => return "return null;".to_owned(),
            _ => {}
        }
    }
    if class == "net/minecraft/world/entity/Entity" && method == "convertTo" {
        return "return null;".to_owned();
    }
    if class == "net/minecraft/world/entity/ai/attributes/Attribute" {
        match (method, ret) {
            // 26.3 names the base value `getDefaultValue`; `getBaseValue` is
            // the Yarn spelling kept for a snapshot-drift synthetic edge.
            ("getDefaultValue", "double") | ("getBaseValue", "double") => {
                return "return horizonBaseValue;".to_owned();
            }
            ("getMinValue", "double") => return "return horizonMinValue;".to_owned(),
            ("getMaxValue", "double") => return "return horizonMaxValue;".to_owned(),
            ("getDescriptionId", "java.lang.String") => {
                return "return horizonDescriptionId;".to_owned();
            }
            ("isClientSyncable", "boolean") => return "return horizonSyncable;".to_owned(),
            ("setSyncable", "net.minecraft.world.entity.ai.attributes.Attribute") => {
                return "horizonSyncable = p0; return this;".to_owned();
            }
            ("setSentiment", "net.minecraft.world.entity.ai.attributes.Attribute") => {
                return "horizonSentiment = p0; return this;".to_owned();
            }
            ("sanitizeValue", "double") => return "return p0;".to_owned(),
            ("getStyle", "net.minecraft.ChatFormatting") => return "return null;".to_owned(),
            _ => {}
        }
    }
    if class == "net/minecraft/world/entity/ai/attributes/RangedAttribute" {
        match (method, ret) {
            ("getDefaultValue", "double") | ("getBaseValue", "double") => {
                return "return horizonBaseValue;".to_owned();
            }
            ("getMinValue", "double") => return "return horizonMinValue;".to_owned(),
            ("getMaxValue", "double") => return "return horizonMaxValue;".to_owned(),
            ("setSyncable", "net.minecraft.world.entity.ai.attributes.Attribute") => {
                return "horizonSyncable = p0; return this;".to_owned();
            }
            ("sanitizeValue", "double") => return "return p0;".to_owned(),
            _ => {}
        }
    }
    if class == "net/minecraft/world/entity/ai/attributes/AttributeSupplier" {
        match (method, ret) {
            // Read the bounded default, not `AttributeInstance.getBaseValue`:
            // that accessor is emitted only when the mod names it, and this
            // body must compile for a supplier-only target too.
            ("getValue", "double") | ("getBaseValue", "double") => return "return 0.0;".to_owned(),
            ("getModifierValue", "double") => return "return 0.0;".to_owned(),
            ("createInstance", "net.minecraft.world.entity.ai.attributes.AttributeInstance") => {
                return "net.minecraft.world.entity.ai.attributes.AttributeInstance value = new net.minecraft.world.entity.ai.attributes.AttributeInstance(); horizonAttributes.put(p1, value); return value;".to_owned();
            }
            ("builder", "net.minecraft.world.entity.ai.attributes.AttributeSupplier.Builder") => {
                return "return new net.minecraft.world.entity.ai.attributes.AttributeSupplier.Builder();".to_owned();
            }
            ("hasAttribute", "boolean") | ("hasModifier", "boolean") => {
                return "return horizonAttributes.containsKey(p0);".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/world/entity/ai/attributes/AttributeSupplier$Builder" {
        match (method, ret) {
            ("add", "net.minecraft.world.entity.ai.attributes.AttributeSupplier.Builder") => {
                return "return this;".to_owned();
            }
            ("build", "net.minecraft.world.entity.ai.attributes.AttributeSupplier")
            | ("create", "net.minecraft.world.entity.ai.attributes.AttributeSupplier") => {
                return "return new net.minecraft.world.entity.ai.attributes.AttributeSupplier();"
                    .to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/world/entity/ai/attributes/AttributeInstance" {
        match (method, ret) {
            ("getBaseValue", "double") => return "return horizonBaseValue;".to_owned(),
            ("getValue", "double") => return "return horizonValue;".to_owned(),
            ("setBaseValue", "void") => {
                return "horizonBaseValue = p0; horizonValue = p0;".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/network/syncher/SynchedEntityData" {
        match (method, ret) {
            ("get", "java.lang.Object") => return "return horizonItems.get(p0);".to_owned(),
            ("set", "void") => return "horizonItems.put(p0, p1); horizonDirty.add(p0);".to_owned(),
            ("define", "void") => return "horizonItems.put(p0, p1);".to_owned(),
            // 26.3's `defineId` allocates the accessor; keep the id monotonic
            // so two definitions in one class do not collide.
            ("defineId", "net.minecraft.network.syncher.EntityDataAccessor") => {
                return "return new net.minecraft.network.syncher.EntityDataAccessor(horizonNextId++, p1);".to_owned();
            }
            ("isDirty", "boolean") => return "return !horizonDirty.isEmpty();".to_owned(),
            ("packDirty", _) => return "java.util.List values = new java.util.ArrayList(); for (Object key : horizonDirty) values.add(horizonItems.get(key)); horizonDirty.clear(); return values;".to_owned(),
            ("getNonDefaultValues", _) => return "java.util.List values = new java.util.ArrayList(); for (Object key : horizonDirty) values.add(horizonItems.get(key)); return values;".to_owned(),
            ("assignValues", "void") => return "horizonAssigned.clear(); if (p0 != null) horizonAssigned.addAll((java.util.List) p0);".to_owned(),
            ("pack", _) => return "return new java.util.ArrayList();".to_owned(),
            ("clearDirty", "void") => return "horizonDirty.clear();".to_owned(),
            _ => {}
        }
    }
    // `EntityDataAccessor` is a record in 26.3: `id()` is its component
    // accessor, and the Yarn `getId` spelling is only a drift edge.
    if class == "net/minecraft/network/syncher/EntityDataAccessor" && method == "getId" {
        return "return id();".to_owned();
    }
    // `DamageType` is a record in 26.3. Its component accessors are generated
    // by the record declaration, so these arms only serve snapshot drift.
    if class == "net/minecraft/world/damagesource/DamageType" {
        match (method, ret) {
            ("msgId", "java.lang.String") => return "return \"generic\";".to_owned(),
            ("exhaustion", "float") => return "return 0.0f;".to_owned(),
            ("scaling", _) | ("effects", _) | ("deathMessageType", _) => {
                return "return null;".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/world/damagesource/DamageSource" {
        match (method, ret) {
            ("type", "net.minecraft.world.damagesource.DamageType") => {
                return "return horizonType;".to_owned();
            }
            ("getEntity", "net.minecraft.world.entity.Entity") => {
                return "return horizonEntity;".to_owned();
            }
            ("getDirectEntity", "net.minecraft.world.entity.Entity") => {
                return "return horizonDirectEntity;".to_owned();
            }
            ("getSourcePosition", "net.minecraft.world.phys.Vec3") => {
                return "return horizonSourcePosition;".to_owned();
            }
            ("sourcePositionRaw", "net.minecraft.world.phys.Vec3") => {
                return "return horizonSourcePosition;".to_owned();
            }
            ("typeHolder", _) => return "return null;".to_owned(),
            ("isDirect", "boolean") => return "return horizonDirectEntity != null;".to_owned(),
            ("getMsgId", "java.lang.String") => {
                return "return horizonType == null ? \"generic\" : horizonType.msgId();"
                    .to_owned();
            }
            ("scalesWithDifficulty", "boolean") | ("isCreativePlayer", "boolean") => {
                return "return false;".to_owned();
            }
            ("getWeaponItem", "net.minecraft.world.item.ItemStack") => {
                return "return new net.minecraft.world.item.ItemStack();".to_owned();
            }
            ("getLocalizedDeathMessage", "net.minecraft.network.chat.Component") => {
                return "return null;".to_owned();
            }
            ("is", "boolean") => return "return false;".to_owned(),
            ("getFoodExhaustion", "float") => {
                return "return horizonType == null ? 0.0f : horizonType.exhaustion();".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/world/level/block/entity/BlockEntity" {
        match (method, ret, descriptor) {
            ("getBlockPos", "net.minecraft.core.BlockPos", "()Lnet/minecraft/core/BlockPos;") => {
                return "return horizonBlockPos;".to_owned();
            }
            (
                "getLevel",
                "net.minecraft.world.level.Level",
                "()Lnet/minecraft/world/level/Level;",
            ) => return "return horizonLevel;".to_owned(),
            ("hasLevel", "boolean", "()Z") => return "return horizonLevel != null;".to_owned(),
            (
                "getBlockState",
                "net.minecraft.world.level.block.state.BlockState",
                "()Lnet/minecraft/world/level/block/state/BlockState;",
            ) => return "return horizonBlockState;".to_owned(),
            ("isRemoved", "boolean", "()Z") => return "return horizonRemoved;".to_owned(),
            ("setRemoved", "void", "()V") => return "horizonRemoved = true;".to_owned(),
            ("setChanged", "void", "()V") => return "".to_owned(),
            (
                "getUpdateTag",
                "net.minecraft.nbt.CompoundTag",
                "()Lnet/minecraft/nbt/CompoundTag;",
            ) => return "return horizonData.copy();".to_owned(),
            ("saveAdditional", "void", "(Lnet/minecraft/nbt/CompoundTag;)V")
            | ("saveWithoutMetadata", "void", "(Lnet/minecraft/nbt/CompoundTag;)V") => {
                return "if (p0 != null) p0.merge(horizonData);".to_owned();
            }
            ("load", "void", "(Lnet/minecraft/nbt/CompoundTag;)V")
            | ("handleUpdateTag", "void", "(Lnet/minecraft/nbt/CompoundTag;)V") => {
                return "if (p0 != null) horizonData = p0.copy();".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/world/entity/LivingEntity" {
        match (method, ret, descriptor) {
            ("getHealth", "float", "()F") => return "return horizonHealth;".to_owned(),
            ("getMaxHealth", "float", "()F") => return "return horizonMaxHealth;".to_owned(),
            ("setHealth", "void", "(F)V") => return "horizonHealth = p0;".to_owned(),
            ("heal", "void", "(F)V") => {
                return "horizonHealth = Math.min(horizonMaxHealth, horizonHealth + p0);"
                    .to_owned();
            }
            ("isDeadOrDying", "boolean", "()Z") => {
                return "return horizonHealth <= 0.0f;".to_owned();
            }
            ("isAlive", "boolean", "()Z") => {
                return "return horizonHealth > 0.0f;".to_owned();
            }
            (
                "getMainHandItem",
                "net.minecraft.world.item.ItemStack",
                "()Lnet/minecraft/world/item/ItemStack;",
            ) => {
                return "return horizonMainHand;".to_owned();
            }
            (
                "getOffhandItem",
                "net.minecraft.world.item.ItemStack",
                "()Lnet/minecraft/world/item/ItemStack;",
            ) => {
                return "return horizonOffhand;".to_owned();
            }
            (
                "getItemInHand",
                "net.minecraft.world.item.ItemStack",
                "(Lnet/minecraft/world/InteractionHand;)Lnet/minecraft/world/item/ItemStack;",
            ) => {
                return "return p0 == net.minecraft.world.InteractionHand.OFF_HAND ? horizonOffhand : horizonMainHand;".to_owned();
            }
            (
                "setItemInHand",
                "void",
                "(Lnet/minecraft/world/InteractionHand;Lnet/minecraft/world/item/ItemStack;)V",
            ) => {
                return "if (p0 == net.minecraft.world.InteractionHand.OFF_HAND) horizonOffhand = p1; else horizonMainHand = p1;".to_owned();
            }
            ("getArmorValue", "int", "()I") => return "return horizonArmorValue;".to_owned(),
            ("addEffect", "boolean", "(Lnet/minecraft/world/effect/MobEffectInstance;)Z") => {
                return "if (p0 == null) return false; horizonEffects.put(p0.getEffect(), p0); return true;".to_owned();
            }
            ("hasEffect", "boolean", "(Lnet/minecraft/world/effect/MobEffect;)Z") => {
                return "return horizonEffects.containsKey(p0);".to_owned();
            }
            (
                "getEffect",
                "net.minecraft.world.effect.MobEffectInstance",
                "(Lnet/minecraft/world/effect/MobEffect;)Lnet/minecraft/world/effect/MobEffectInstance;",
            ) => {
                return "return horizonEffects.get(p0);".to_owned();
            }
            ("removeEffect", "boolean", "(Lnet/minecraft/world/effect/MobEffect;)Z") => {
                return "return horizonEffects.remove(p0) != null;".to_owned();
            }
            ("removeAllEffects", "boolean", "()Z") => {
                return "boolean hadEffects = !horizonEffects.isEmpty(); horizonEffects.clear(); return hadEffects;".to_owned();
            }
            // `getAttribute`/`getAttributes` are declared here in 26.3, so the
            // common receiver type is `LivingEntity`, not only `Mob`.
            ("getAttribute", "net.minecraft.world.entity.ai.attributes.AttributeInstance", _) => {
                return "return horizonAttributes.get(p0);".to_owned();
            }
            ("getAttributes", "net.minecraft.world.entity.ai.attributes.AttributeMap", _) => {
                return "return horizonAttributeMap;".to_owned();
            }
            ("getAttributeValue", "double", _) => return "return 0.0;".to_owned(),
            ("getAttributeBaseValue", "double", _) => return "return 0.0;".to_owned(),
            _ => {}
        }
    }
    if class == "net/minecraft/world/effect/MobEffectInstance" {
        match (method, ret, descriptor) {
            (
                "getEffect",
                "net.minecraft.world.effect.MobEffect",
                "()Lnet/minecraft/world/effect/MobEffect;",
            ) => return "return horizonEffect;".to_owned(),
            ("getDuration", "int", "()I") => return "return horizonDuration;".to_owned(),
            ("getAmplifier", "int", "()I") => return "return horizonAmplifier;".to_owned(),
            ("isAmbient", "boolean", "()Z") => return "return horizonAmbient;".to_owned(),
            ("isVisible", "boolean", "()Z") => return "return horizonVisible;".to_owned(),
            ("showIcon", "boolean", "()Z") => return "return horizonShowIcon;".to_owned(),
            ("isInfiniteDuration", "boolean", "()Z") => {
                return "return horizonDuration == -1;".to_owned();
            }
            ("tickDownDuration", "int", "()I") => {
                return "if (horizonDuration > 0) horizonDuration--; return horizonDuration;"
                    .to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/world/entity/player/Inventory" {
        match (method, ret, descriptor) {
            ("getContainerSize", "int", "()I") => return "return horizonItems.length;".to_owned(),
            ("isEmpty", "boolean", "()Z") => {
                return "for (net.minecraft.world.item.ItemStack stack : horizonItems) if (stack != null && !stack.isEmpty()) return false; return true;".to_owned();
            }
            (
                "getItem",
                "net.minecraft.world.item.ItemStack",
                "(I)Lnet/minecraft/world/item/ItemStack;",
            ) => {
                return "return p0 >= 0 && p0 < horizonItems.length && horizonItems[p0] != null ? horizonItems[p0] : new net.minecraft.world.item.ItemStack();".to_owned();
            }
            ("setItem", "void", "(ILnet/minecraft/world/item/ItemStack;)V") => {
                return "if (p0 >= 0 && p0 < horizonItems.length) horizonItems[p0] = p1 == null ? new net.minecraft.world.item.ItemStack() : p1;".to_owned();
            }
            (
                "getSelected",
                "net.minecraft.world.item.ItemStack",
                "()Lnet/minecraft/world/item/ItemStack;",
            ) => {
                return "return getItem(horizonSelected);".to_owned();
            }
            (
                "removeItem",
                "net.minecraft.world.item.ItemStack",
                "(II)Lnet/minecraft/world/item/ItemStack;",
            ) => {
                return "if (p0 < 0 || p0 >= horizonItems.length) return new net.minecraft.world.item.ItemStack(); net.minecraft.world.item.ItemStack old = horizonItems[p0]; horizonItems[p0] = new net.minecraft.world.item.ItemStack(); return old == null ? new net.minecraft.world.item.ItemStack() : old;".to_owned();
            }
            (
                "removeItemNoUpdate",
                "net.minecraft.world.item.ItemStack",
                "(I)Lnet/minecraft/world/item/ItemStack;",
            ) => {
                return "return removeItem(p0, Integer.MAX_VALUE);".to_owned();
            }
            ("clearContent", "void", "()V") => {
                return "for (int i = 0; i < horizonItems.length; i++) horizonItems[i] = new net.minecraft.world.item.ItemStack();".to_owned();
            }
            ("setChanged", "void", "()V") => return "".to_owned(),
            _ => {}
        }
    }
    if class == "net/minecraft/world/entity/player/Abilities" {
        match (method, ret, descriptor) {
            ("getFlyingSpeed", "float", "()F") => return "return horizonFlyingSpeed;".to_owned(),
            ("setFlyingSpeed", "void", "(F)V") => return "horizonFlyingSpeed = p0;".to_owned(),
            ("getWalkingSpeed", "float", "()F") => return "return horizonWalkingSpeed;".to_owned(),
            ("setWalkingSpeed", "void", "(F)V") => return "horizonWalkingSpeed = p0;".to_owned(),
            _ => {}
        }
    }
    if matches!(
        class,
        "net/minecraft/world/entity/player/Player" | "net/minecraft/server/level/ServerPlayer"
    ) {
        match (method, ret, descriptor) {
            ("isCreative", "boolean", "()Z") => {
                return "return horizonCreative;".to_owned();
            }
            ("isSpectator", "boolean", "()Z") => {
                return "return horizonSpectator;".to_owned();
            }
            ("getScoreboardName", "java.lang.String", "()Ljava/lang/String;") => {
                return "return horizonScoreboardName;".to_owned();
            }
            (
                "getName",
                "net.minecraft.network.chat.Component",
                "()Lnet/minecraft/network/chat/Component;",
            )
            | (
                "getDisplayName",
                "net.minecraft.network.chat.Component",
                "()Lnet/minecraft/network/chat/Component;",
            ) => {
                return "return net.minecraft.network.chat.Component.literal(horizonScoreboardName);"
                    .to_owned();
            }
            ("getExperienceLevel", "int", "()I") => {
                return "return horizonExperienceLevel;".to_owned();
            }
            ("getExperienceProgress", "float", "()F") => {
                return "return horizonExperienceProgress;".to_owned();
            }
            (
                "getInventory",
                "net.minecraft.world.entity.player.Inventory",
                "()Lnet/minecraft/world/entity/player/Inventory;",
            ) => {
                return "return horizonInventory;".to_owned();
            }
            (
                "getAbilities",
                "net.minecraft.world.entity.player.Abilities",
                "()Lnet/minecraft/world/entity/player/Abilities;",
            ) => {
                return "return horizonAbilities;".to_owned();
            }
            _ => {}
        }
    }
    if class == "net/minecraft/resources/Identifier"
        && ret == "net.minecraft.resources.Identifier"
        && matches!(
            method,
            "fromNamespaceAndPath"
                | "withDefaultNamespace"
                | "tryBuild"
                | "bySeparator"
                | "tryBySeparator"
        )
    {
        return match method {
            "fromNamespaceAndPath" | "tryBuild" => "return new Identifier(p0, p1);".to_owned(),
            "withDefaultNamespace" => "return new Identifier(\"minecraft\", p0);".to_owned(),
            _ => "return new Identifier();".to_owned(),
        };
    }
    if class == "net/minecraft/resources/Identifier" {
        return match method {
            "parse" | "tryParse" => "int separator = p0.indexOf(':'); return separator < 0 ? new Identifier(\"minecraft\", p0) : new Identifier(p0.substring(0, separator), p0.substring(separator + 1));".to_owned(),
            "getNamespace" => "return horizonNamespace;".to_owned(),
            "getPath" => "return horizonPath;".to_owned(),
            "withPath" => "return new Identifier(horizonNamespace, (String) p0.apply(horizonPath));".to_owned(),
            "withPrefix" => "return new Identifier(horizonNamespace, p0 + horizonPath);".to_owned(),
            "withSuffix" => "return new Identifier(horizonNamespace, horizonPath + p0);".to_owned(),
            "toString" => "return horizonNamespace + \":\" + horizonPath;".to_owned(),
            "compareTo" if ret == "int" => "return toString().compareTo(p0.toString());".to_owned(),
            "toLanguageKey" if descriptor == "(Ljava/lang/String;)Ljava/lang/String;" => "return p0 + \".\" + horizonNamespace + \".\" + horizonPath.replace('/', '.');".to_owned(),
            "toLanguageKey" => "return horizonNamespace + \".\" + horizonPath.replace('/', '.');".to_owned(),
            "toShortString" => "return horizonNamespace.equals(\"minecraft\") ? horizonPath : horizonNamespace + \"/\" + horizonPath;".to_owned(),
            "hashCode" => {
                "return java.util.Objects.hash(horizonNamespace, horizonPath);".to_owned()
            }
            "equals" => {
                "return p0 instanceof Identifier other && horizonNamespace.equals(other.horizonNamespace) && horizonPath.equals(other.horizonPath);".to_owned()
            }
            _ => format!(
                "throw new UnsupportedOperationException(\"horizon stub: {class}#{method}\");"
            ),
        };
    }
    format!("throw new UnsupportedOperationException(\"horizon stub: {class}#{method}\");")
}

/// The generated `StreamCodec`'s bodies: the wire form of a codec, not a
/// placeholder object.
///
/// A codec built by `of` keeps the mod's encoder and decoder, `unit` writes
/// nothing, and `map`, `dispatch`, `composite` and `recursive` keep their
/// members so `encode`/`decode` descend into them in order. A composite
/// encodes each member through the getter it was given and rebuilds the value
/// from the members' own decode results, so it never has to call the record's
/// constructor reflectively: a decoded member pair that a constructor would
/// canonicalise (a defensive copy, a clamp) differs from what vanilla's
/// `composite` returns too, since vanilla's own constructor call is the same
/// kind of conversion done once on each side of the wire.
///
/// `None` means the method has no bounded body here, and the façade keeps the
/// shape-only default.
fn stream_codec_body(method: &str, descriptor: &str) -> Option<String> {
    const CODEC: &str = "net.minecraft.network.codec.StreamCodec";
    match (method, descriptor) {
        // `new StreamCodec<>() { public V decode(B b) {...} public void
        // encode(B b, V v) {...} }`: an anonymous body's two callbacks are not
        // recovered from the class file yet, so this codec writes nothing and
        // answers null. `StreamCodec.of(...)` and `unit` keep working.
        ("<init>", "(Lio/netty/buffer/ByteBuf;)V" | "()V") => Some(String::new()),
        (
            "of",
            "(Lnet/minecraft/network/codec/StreamEncoder;Lnet/minecraft/network/codec/StreamDecoder;)Lnet/minecraft/network/codec/StreamCodec;",
        ) => Some(format!(
            "{CODEC} codec = new {CODEC}(); return codec.horizonOf(p0, p1);"
        )),
        // `ofMember` is the same pair with the receiver's value in front of the
        // encoder's argument, which the bounded buffer pair does not carry.
        (
            "ofMember",
            "(Lnet/minecraft/network/codec/StreamMemberEncoder;Lnet/minecraft/network/codec/StreamDecoder;)Lnet/minecraft/network/codec/StreamCodec;",
        ) => Some(format!(
            "{CODEC} codec = new {CODEC}(); return codec.horizonOf(null, p1);"
        )),
        // Nothing is written, and decoding answers the same instance.
        ("unit", _) => Some(format!(
            "{CODEC} codec = new {CODEC}(); codec.horizonValue = p0; return codec;"
        )),
        // `apply` hands the mod's `CodecOperation` the receiver, which builds a
        // real codec of its own. The rest take values the bounded buffer does
        // not model (a retyped buffer, a null-flag wrapper), so the receiver
        // keeps answering for the value it already holds.
        ("apply", _) if descriptor.ends_with("Lnet/minecraft/network/codec/StreamCodec;") => {
            Some("return p0 == null ? null : p0.apply(this);".to_owned())
        }
        ("mapStream" | "cast" | "optional" | "nullable" | "tuple", _)
            if descriptor.ends_with("Lnet/minecraft/network/codec/StreamCodec;") =>
        {
            Some("return this;".to_owned())
        }
        (
            "map",
            "(Ljava/util/function/Function;Ljava/util/function/Function;)Lnet/minecraft/network/codec/StreamCodec;",
        ) => Some(format!(
            // Vanilla's `map(to, from)`: `to` turns the decoded value into the
            // codec's value, `from` turns the codec's value back for encoding.
            "{CODEC} codec = new {CODEC}(); codec.horizonKind = HORIZON_MAP; codec.horizonMember1 = this; codec.horizonFirst = p1; codec.horizonSecond = p0; return codec;"
        )),
        (
            "dispatch",
            "(Ljava/util/function/Function;Ljava/util/function/Function;)Lnet/minecraft/network/codec/StreamCodec;",
        ) => Some(format!(
            "{CODEC} codec = new {CODEC}(); codec.horizonKind = HORIZON_DISPATCH; codec.horizonMember1 = this; codec.horizonFirst = p0; codec.horizonSecond = p1; return codec;"
        )),
        (
            "recursive",
            "(Ljava/util/function/UnaryOperator;)Lnet/minecraft/network/codec/StreamCodec;",
        ) => Some(format!(
            "{CODEC} codec = new {CODEC}(); codec.horizonKind = HORIZON_RECURSIVE; codec.horizonFirst = p0; return codec;"
        )),
        ("composite", _) if descriptor.ends_with("Lnet/minecraft/network/codec/StreamCodec;") => {
            Some(stream_composite_body(descriptor))
        }
        ("encode", _) => Some(ENCODE_CODEC.to_owned()),
        ("decode", _) => Some(DECODE_CODEC.to_owned()),
        _ if descriptor.ends_with("Lnet/minecraft/network/codec/StreamCodec;") => {
            Some(format!("{CODEC} codec = new {CODEC}(); return codec;"))
        }
        _ => None,
    }
}

/// `StreamCodec#encode`, in every form the factories build. The heads are in
/// the same order `decode` reads them back.
const ENCODE_CODEC: &str = r#"if (horizonKind == HORIZON_MAP) { horizonMember1.encode(p0, horizonFirst.apply(p1)); return; }
        if (horizonKind == HORIZON_DISPATCH) { Object type = horizonFirst.apply(p1); horizonMember1.encode(p0, type); (horizonDispatched(type)).encode(p0, p1); return; }
        if (horizonKind == HORIZON_RECURSIVE) { if (horizonRecursiveCodec == null) horizonRecursiveCodec = horizonRecursive(this); horizonRecursiveCodec.encode(p0, p1); return; }
        if (horizonKind == HORIZON_COMPOSITE) { for (int i = 1; i <= 8; i++) { net.minecraft.network.codec.StreamCodec member = horizonMemberAt(i); if (member == null) break; member.encode(p0, horizonGetterAt(i).apply(p1)); } return; }
        if (horizonValue != null) return;
        if (horizonFirst != null) { horizonFirst.apply(new Object[] { p0, p1 }); return; }"#;

/// `StreamCodec#decode`, reading the heads `ENCODE_CODEC` wrote.
const DECODE_CODEC: &str = r#"if (horizonKind == HORIZON_MAP) return horizonSecond.apply(horizonMember1.decode(p0));
        if (horizonKind == HORIZON_DISPATCH) { Object type = horizonMember1.decode(p0); return (horizonDispatched(type)).decode(p0); }
        if (horizonKind == HORIZON_RECURSIVE) { if (horizonRecursiveCodec == null) horizonRecursiveCodec = horizonRecursive(this); return horizonRecursiveCodec.decode(p0); }
        if (horizonKind == HORIZON_COMPOSITE) return horizonConstruct(horizonDecodeMembers(p0));
        if (horizonValue != null) return horizonValue;
        if (horizonSecond != null && p0 != null) return horizonSecond.apply(p0);
        return null;"#;

/// `StreamCodec.composite`'s body: the members in wire order, each with the
/// getter that reads its value off the composite.
///
/// Vanilla writes member 1, then member 2, and so on; the façade keeps the
/// same order.
fn stream_composite_body(descriptor: &str) -> String {
    const CODEC: &str = "net.minecraft.network.codec.StreamCodec";
    // (StreamCodec, Function) repeated, then the constructor function.
    let args = descriptor_parameters(descriptor);
    let pairs = args.len() / 2;
    let mut body = format!("{CODEC} codec = new {CODEC}(); codec.horizonKind = HORIZON_COMPOSITE;");
    for member in 0..pairs.min(8) {
        body.push_str(&format!(
            " codec.horizonMember{} = p{};",
            member + 1,
            member * 2
        ));
        body.push_str(&format!(
            " codec.{} = p{};",
            stream_composite_getter(member + 1),
            member * 2 + 1
        ));
    }
    body.push_str(&format!(
        " codec.horizonConstructor = p{}; return codec;",
        pairs * 2
    ));
    body
}

/// The field a composite member's getter is kept in.
fn stream_composite_getter(index: usize) -> &'static str {
    const FIELDS: [&str; 8] = [
        "horizonGetters1",
        "horizonGetters2",
        "horizonGetters3",
        "horizonGetters4",
        "horizonGetters5",
        "horizonGetters6",
        "horizonGetters7",
        "horizonGetters8",
    ];
    FIELDS[index - 1]
}

/// The parameter descriptors of a method descriptor, in order.
fn descriptor_parameters(descriptor: &str) -> Vec<String> {
    let Some(end) = descriptor.find(')') else {
        return Vec::new();
    };
    let args = &descriptor[1..end];
    let bytes = args.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let start = at;
        while at < bytes.len() && bytes[at] == b'[' {
            at += 1;
        }
        if at >= bytes.len() {
            break;
        }
        if bytes[at] == b'L' {
            while at < bytes.len() && bytes[at] != b';' {
                at += 1;
            }
            at += 1;
        } else {
            at += 1;
        }
        out.push(args[start..at].to_owned());
    }
    out
}

/// `ByteBufCodecs`' factories as real codecs over the bounded
/// `FriendlyByteBuf`: the primitive pairs, a length-prefixed list (count as a
/// varint, then element after element), an optional (a presence byte), and the
/// buffer-time factory constant that runs a mod's own function.
///
/// A factory this slice does not model returns None, and the façade keeps a
/// shape-only codec for that call, so a payload that needs one still links.
fn byte_buf_codec_factory(method: &str, descriptor: &str) -> Option<String> {
    const CODEC: &str = "net.minecraft.network.codec.StreamCodec";
    // (wire type, read call, write call): the pairs the buffer already
    // implements, so the codec is a direct `of(...)` over them.
    // (boxed value type, read call, write call): the pair the buffer already
    // implements. Reads answer the boxed value; writes take the boxed one and
    // unbox it.
    let pair = match method {
        "BOOL" => Some(("java.lang.Boolean", "readBoolean", "writeBoolean")),
        "BYTE" => Some(("java.lang.Byte", "readByte", "writeByte")),
        "SHORT" => Some(("java.lang.Short", "readShort", "writeShort")),
        "UNSIGNED_SHORT" => Some(("java.lang.Integer", "readUnsignedShort", "writeShort")),
        "INT" | "VAR_INT" => Some(("java.lang.Integer", "readVarInt", "writeVarInt")),
        "LONG" | "VAR_LONG" => Some(("java.lang.Long", "readVarLong", "writeVarLong")),
        "FLOAT" => Some(("java.lang.Float", "readFloat", "writeFloat")),
        "DOUBLE" => Some(("java.lang.Double", "readDouble", "writeDouble")),
        "BYTE_ARRAY" => Some(("byte[]", "readByteArray", "writeByteArray")),
        "STRING_UTF8" => Some(("java.lang.String", "readUtf", "writeUtf")),
        "IDENTIFIER" => Some((
            "net.minecraft.resources.Identifier",
            "readIdentifier",
            "writeIdentifier",
        )),
        _ => None,
    };
    if let Some((value, read, write)) = pair {
        // Explicit types on both callbacks: the façade's `ByteBufCodecs` is
        // raw, so a lambda's parameters would infer `Object` from `of`'s type
        // variables and neither the buffer call nor the unboxing would resolve.
        return Some(format!(
            "return {CODEC}.of((net.minecraft.network.FriendlyByteBuf buffer, {value} value) -> buffer.{write}(value), (net.minecraft.network.FriendlyByteBuf buffer) -> ({value}) buffer.{read}());"
        ));
    }
    match method {
        // A list is the element count as a varint, then element after element.
        "list" if descriptor.contains("Ljava/util/function/IntFunction;") => Some(format!(
            "return {CODEC}.of((net.minecraft.network.FriendlyByteBuf buffer, Object value) -> {{ java.util.List elements = (java.util.List) value; buffer.writeVarInt(elements.size()); for (Object element : elements) p0.encode(buffer, element); }}, (net.minecraft.network.FriendlyByteBuf buffer) -> {{ int size = buffer.readVarInt(); java.util.List values = (java.util.List) p1.apply(0); for (int i = 0; i < size; i++) values.add(p0.decode(buffer)); return values; }});"
        )),
        "list" => Some(format!(
            "return {CODEC}.of((net.minecraft.network.FriendlyByteBuf buffer, Object value) -> {{ java.util.List elements = (java.util.List) value; buffer.writeVarInt(elements.size()); for (Object element : elements) p0.encode(buffer, element); }}, (net.minecraft.network.FriendlyByteBuf buffer) -> {{ int size = buffer.readVarInt(); java.util.List values = new java.util.ArrayList(); for (int i = 0; i < size; i++) values.add(p0.decode(buffer)); return values; }});"
        )),
        "optional" => Some(format!(
            "return {CODEC}.of((net.minecraft.network.FriendlyByteBuf buffer, Object value) -> {{ java.util.Optional optional = (java.util.Optional) value; buffer.writeBoolean(optional.isPresent()); if (optional.isPresent()) p0.encode(buffer, optional.get()); }}, (net.minecraft.network.FriendlyByteBuf buffer) -> buffer.readBoolean() ? java.util.Optional.ofNullable(p0.decode(buffer)) : java.util.Optional.empty());"
        )),
        // The `ByteBufCodecs` constant that forwards to a buffer-time factory:
        // the mod's function runs on the bounded buffer and must answer a real
        // codec, which `of(...)` and this slice's factories already do.
        "fromCodec" => Some(format!(
            "Object made = p0.apply(new net.minecraft.network.FriendlyByteBuf()); if (made instanceof {CODEC}) return ({CODEC}) made; {CODEC} fallback = new {CODEC}(); return fallback;"
        )),
        _ => None,
    }
}

fn voxel_shape_placeholder() -> String {
    "return new net.minecraft.world.phys.shapes.VoxelShape(null) {\n".to_owned()
        + "    public it.unimi.dsi.fastutil.doubles.DoubleList getCoords(net.minecraft.core.Direction.Axis axis) {\n"
        + "        return it.unimi.dsi.fastutil.doubles.DoubleLists.EMPTY_LIST;\n"
        + "    }\n"
        + "};"
}

#[cfg(test)]
fn runtime_field_default(class: &str, descriptor: &str, fallback: &str) -> String {
    runtime_field_default_named(class, "", descriptor, fallback)
}

fn runtime_field_default_named(
    class: &str,
    field: &str,
    descriptor: &str,
    fallback: &str,
) -> String {
    if class == "net/minecraft/world/InteractionResult" {
        return match field {
            "SUCCESS" => "new net.minecraft.world.InteractionResult.Success(net.minecraft.world.InteractionResult.SwingSource.PREDICTED, net.minecraft.world.InteractionResult.ItemContext.DEFAULT)".to_owned(),
            "SUCCESS_SERVER" => "new net.minecraft.world.InteractionResult.Success(net.minecraft.world.InteractionResult.SwingSource.SERVER_ONLY, net.minecraft.world.InteractionResult.ItemContext.DEFAULT)".to_owned(),
            "CONSUME" => "new net.minecraft.world.InteractionResult.Success(net.minecraft.world.InteractionResult.SwingSource.NONE, net.minecraft.world.InteractionResult.ItemContext.DEFAULT)".to_owned(),
            "FAIL" => "new net.minecraft.world.InteractionResult.Fail()".to_owned(),
            "PASS" => "new net.minecraft.world.InteractionResult.Pass()".to_owned(),
            "TRY_WITH_EMPTY_HAND" => "new net.minecraft.world.InteractionResult.TryEmptyHandInteraction()".to_owned(),
            _ => fallback.to_owned(),
        };
    }
    if class == "net/minecraft/world/InteractionResult$ItemContext" {
        return match field {
            "DEFAULT" => {
                "new net.minecraft.world.InteractionResult.ItemContext(true, null)".to_owned()
            }
            "NONE" => {
                "new net.minecraft.world.InteractionResult.ItemContext(false, null)".to_owned()
            }
            _ => fallback.to_owned(),
        };
    }
    if class == "net/minecraft/world/entity/SpawnPlacementTypes"
        && descriptor == "Lnet/minecraft/world/entity/SpawnPlacementType;"
    {
        return "new net.minecraft.world.entity.SpawnPlacementType() { public boolean isSpawnPositionOk(net.minecraft.world.level.LevelReader level, net.minecraft.core.BlockPos pos, net.minecraft.world.entity.EntityType type) { return true; } }".to_owned();
    }
    if class == "net/minecraft/core/registries/BuiltInRegistries"
        && descriptor == "Lnet/minecraft/core/Registry;"
    {
        return "new net.minecraft.core.Registry() {}".to_owned();
    }
    if class == "net/minecraft/core/registries/BuiltInRegistries"
        && descriptor == "Lnet/minecraft/core/DefaultedRegistry;"
    {
        return "new net.minecraft.core.DefaultedRegistry() {}".to_owned();
    }
    if class == "net/minecraft/core/component/DataComponents"
        && descriptor == "Lnet/minecraft/core/component/DataComponentType;"
    {
        // A distinct object per constant makes it a usable identity key for
        // ItemStack's erased component map (the real type is also identity
        // based), while keeping the generated façade independent of codecs.
        return "new net.minecraft.core.component.DataComponentType() {}".to_owned();
    }
    if class == "net/minecraft/resources/RegistryDataLoader" && descriptor == "Ljava/util/List;" {
        return "new java.util.ArrayList()".to_owned();
    }
    match descriptor {
        "Lnet/minecraft/network/codec/StreamCodec;" => {
            "new net.minecraft.network.codec.StreamCodec()".to_owned()
        }
        "Lcom/mojang/serialization/Codec;" | "Lcom/mojang/serialization/codecs/PrimitiveCodec;" => {
            "com.mojang.serialization.Codec.STRING".to_owned()
        }
        "Lcom/mojang/serialization/MapCodec;" => {
            "com.mojang.serialization.Codec.STRING.fieldOf(\"\")".to_owned()
        }
        _ => fallback.to_owned(),
    }
}

fn minimal(name: &str) -> String {
    let (package, simple) = match name.rsplit_once('/') {
        Some((package, simple)) => (Some(package), simple),
        None => (None, name),
    };
    let mut out = String::new();
    if let Some(package) = package {
        let _ = writeln!(out, "package {};", package.replace('/', "."));
    }
    let _ = writeln!(out, "public class {simple} {{ public {simple}() {{}} }}");
    out
}

/// `net/minecraft/world/item/Item$Properties` → `net.minecraft.world.item.Item.Properties`.
fn java_name(name: &str) -> String {
    name.replace(['/', '$'], ".")
}

fn annotation_simple_name(name: &str) -> &str {
    name.trim_start_matches('L')
        .trim_end_matches(';')
        .rsplit('/')
        .next()
        .unwrap_or(name)
}

fn field_is_enum_abi(field: &Declared) -> bool {
    field.is_static() && !field.is_enum_constant()
}

fn default_value_for_java_type(ty: &str) -> String {
    match ty {
        "boolean" => "false".to_owned(),
        "byte" | "char" | "short" | "int" | "long" | "float" | "double" => "0".to_owned(),
        "java.lang.Thread" => "java.lang.Thread.currentThread()".to_owned(),
        "java.lang.String" => "\"\"".to_owned(),
        "java.util.Optional" => "java.util.Optional.empty()".to_owned(),
        "java.util.List" | "java.util.Collection" => "java.util.Collections.emptyList()".to_owned(),
        "java.util.Set" => "java.util.Collections.emptySet()".to_owned(),
        "java.util.Map" => "java.util.Collections.emptyMap()".to_owned(),
        "java.util.Queue" | "java.util.Deque" => "new java.util.ArrayDeque<>()".to_owned(),
        ty if ty.ends_with("[]") && !ty[..ty.len() - 2].ends_with("[]") => {
            format!("new {}[0]", &ty[..ty.len() - 2])
        }
        _ => "null".to_owned(),
    }
}

fn safe_mixin_method(method: &Declared) -> bool {
    let Some(end) = method.descriptor.find(')') else {
        return false;
    };
    let return_descriptor = &method.descriptor[end + 1..];
    matches!(
        return_descriptor.as_bytes().first(),
        Some(b'V' | b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D')
    ) || return_descriptor.starts_with("Ljava/")
        // Array accessors are common for model parts and registry tables. The
        // generated body can safely return an empty one-dimensional array (or
        // null for nested arrays), while the descriptor still preserves the
        // castable JVM ABI.
        || return_descriptor.starts_with('[')
}

fn java_type(descriptor: &str) -> String {
    match descriptor {
        "V" => "void".to_owned(),
        "Z" => "boolean".to_owned(),
        "B" => "byte".to_owned(),
        "C" => "char".to_owned(),
        "S" => "short".to_owned(),
        "I" => "int".to_owned(),
        "J" => "long".to_owned(),
        "F" => "float".to_owned(),
        "D" => "double".to_owned(),
        d if d.starts_with('L') => java_name(d[1..d.len() - 1].trim_end_matches(';')),
        d if d.starts_with('[') => format!("{}[]", java_type(&d[1..])),
        d => d.to_owned(),
    }
}

fn default_value(descriptor: &str) -> &'static str {
    match descriptor {
        "Z" => "false",
        "B" | "C" | "S" | "I" | "J" | "F" | "D" => "0",
        _ => "null",
    }
}

fn constructor_default_value(descriptor: &str) -> &'static str {
    match descriptor {
        "B" => "(byte) 0",
        "C" => "'\\0'",
        "S" => "(short) 0",
        "J" => "0L",
        "F" => "0f",
        "D" => "0d",
        _ => default_value(descriptor),
    }
}

/// Every object type a descriptor names: `(Lnet/minecraft/core/BlockPos;)V`.
fn descriptor_types(descriptor: &str) -> Vec<String> {
    let bytes = descriptor.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'L' {
            let start = at + 1;
            while at < bytes.len() && bytes[at] != b';' {
                at += 1;
            }
            out.push(descriptor[start..at].to_owned());
        }
        at += 1;
    }
    out
}

/// `(Ljava/lang/String;I)V` → `String p0, int p1`.
fn parameters(descriptor: &str) -> String {
    let Some(end) = descriptor.find(')') else {
        return String::new();
    };
    let args = &descriptor[1..end];
    let mut out = Vec::new();
    let mut at = 0;
    let bytes = args.as_bytes();
    while at < bytes.len() {
        let start = at;
        while at < bytes.len() && bytes[at] == b'[' {
            at += 1;
        }
        if at >= bytes.len() {
            break;
        }
        if bytes[at] == b'L' {
            while at < bytes.len() && bytes[at] != b';' {
                at += 1;
            }
            at += 1;
        } else {
            at += 1;
        }
        out.push(format!("{} p{}", java_type(&args[start..at]), out.len()));
    }
    out.join(", ")
}

/// `(Ljava/lang/String;I)V` → `(void, String p0, int p1)`.
fn signature(descriptor: &str) -> (String, String) {
    let Some(end) = descriptor.find(')') else {
        return ("void".to_owned(), String::new());
    };
    (java_type(&descriptor[end + 1..]), parameters(descriptor))
}

/// Every jar under a folder (the client's `libraries/`).
pub fn library_jars(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "jar") {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_become_java() {
        assert_eq!(
            parameters("(Ljava/lang/String;I)V"),
            "java.lang.String p0, int p1"
        );
        assert_eq!(
            parameters("([Lnet/minecraft/core/BlockPos;Z)V"),
            "net.minecraft.core.BlockPos[] p0, boolean p1"
        );
        assert_eq!(
            signature("(I)Lnet/minecraft/world/item/ItemStack;"),
            (
                "net.minecraft.world.item.ItemStack".to_owned(),
                "int p0".to_owned()
            )
        );
        assert_eq!(java_type("[[D"), "double[][]");
        assert_eq!(
            java_name("net/minecraft/world/item/Item$Properties"),
            "net.minecraft.world.item.Item.Properties"
        );
        assert_eq!(
            descriptor_types(
                "(Lnet/minecraft/world/entity/Mob;Lnet/minecraft/world/entity/ConversionParams;)V"
            ),
            vec![
                "net/minecraft/world/entity/Mob".to_owned(),
                "net/minecraft/world/entity/ConversionParams".to_owned()
            ]
        );
    }

    #[test]
    fn neoforge_registration_calls_have_neutral_runtime_values() {
        assert_eq!(
            runtime_body(
                "net/neoforged/fml/javafmlmod/FMLJavaModLoadingContext",
                "get",
                "net.neoforged.fml.javafmlmod.FMLJavaModLoadingContext"
            ),
            "return new FMLJavaModLoadingContext();"
        );
        assert_eq!(
            runtime_body(
                "net/neoforged/fml/javafmlmod/FMLJavaModLoadingContext",
                "getModEventBus",
                "net.neoforged.bus.api.IEventBus"
            ),
            "return new net.neoforged.bus.api.IEventBus() {};"
        );
        assert_eq!(
            runtime_body(
                "net/neoforged/fml/javafmlmod/FMLJavaModLoadingContext",
                "getContainer",
                "net.neoforged.fml.ModContainer"
            ),
            "return new net.neoforged.fml.ModContainer();"
        );
        assert_eq!(
            runtime_body(
                "net/neoforged/bus/api/IEventBus",
                "addListener",
                "net.neoforged.bus.api.IEventBus"
            ),
            "return this;"
        );
        assert_eq!(
            runtime_body(
                "net/neoforged/neoforge/registries/DeferredRegister",
                "create",
                "net.neoforged.neoforge.registries.DeferredRegister"
            ),
            "return new DeferredRegister();"
        );
        assert_eq!(
            runtime_body(
                "net/neoforged/neoforge/registries/DeferredRegister",
                "register",
                "net.neoforged.neoforge.registries.DeferredHolder"
            ),
            "return new net.neoforged.neoforge.registries.DeferredHolder();"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/neoforged/neoforge/registries/DeferredHolder",
                "isPresent",
                "boolean",
                "()Z",
            ),
            "return get() != null;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/neoforged/neoforge/registries/DeferredHolder",
                "getOptional",
                "java.util.Optional",
                "()Ljava/util/Optional;",
            ),
            "return java.util.Optional.ofNullable(get());"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/neoforged/neoforge/registries/DeferredHolder",
                "getId",
                "net.minecraft.resources.Identifier",
                "()Lnet/minecraft/resources/Identifier;",
            ),
            "return horizonId;"
        );
        assert_eq!(
            runtime_body(
                "net/neoforged/neoforge/registries/RegisterEvent",
                "getRegistryKey",
                "net.minecraft.resources.ResourceKey"
            ),
            "return horizonRegistryKey;"
        );
        assert_eq!(
            runtime_body(
                "net/neoforged/neoforge/registries/RegisterEvent",
                "getRegistry",
                "net.minecraft.core.Registry"
            ),
            "return horizonRegistry;"
        );
        assert_eq!(
            runtime_body(
                "net/neoforged/neoforge/network/event/RegisterPayloadHandlersEvent",
                "registrar",
                "net.neoforged.neoforge.network.registration.PayloadRegistrar"
            ),
            "return new net.neoforged.neoforge.network.registration.PayloadRegistrar();"
        );
        assert_eq!(
            runtime_body(
                "net/neoforged/neoforge/network/registration/PayloadRegistrar",
                "playToServer",
                "net.neoforged.neoforge.network.registration.PayloadRegistrar"
            ),
            "return this;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraftforge/network/NetworkRegistry",
                "newSimpleChannel",
                "net.minecraftforge.network.simple.SimpleChannel"
            ),
            "return new net.minecraftforge.network.simple.SimpleChannel();"
        );
        assert_eq!(
            runtime_body(
                "net/minecraftforge/network/simple/SimpleChannel",
                "registerMessage",
                "net.minecraftforge.network.simple.SimpleChannel"
            ),
            "return this;"
        );
    }

    #[test]
    fn neoforge_payload_registrar_retains_registration_state() {
        let source = include_str!("facade.rs");
        assert!(source.contains("private int horizonRegistrationCount;"));
        assert!(source.contains("horizonRegistrationCount++; return this;"));
        assert!(source.contains("private final PayloadRegistrar horizonRegistrar"));
        assert!(source.contains("return horizonRegistrar;"));
        assert!(source.contains("default IEventBus unregister(Object object) { return this; }"));
        // Each direction keeps the payload types it was handed, so bootstrap
        // can read back the registered ids without a live channel.
        assert!(source.contains(
            "private static final java.util.Map<String, java.util.List<Object>> horizonPayloads"
        ));
        for direction in [
            "playToServer",
            "playToClient",
            "playBidirectional",
            "configurationToServer",
            "configurationToClient",
            "configurationBidirectional",
        ] {
            let retained = format!("horizonRetain(\"{direction}\", type, codec)");
            assert!(
                source.contains(&retained),
                "missing direction: {directive}",
                directive = direction
            );
        }
        assert!(source.contains(
            "try { return type.getClass().getMethod(\"id\").invoke(type); } catch (Throwable ignored) { return null; }"
        ));
        assert!(source.contains(
            "public PayloadRegistrar optional() { horizonIsOptional = true; return this; }"
        ));
        assert!(source.contains("public PayloadRegistrar versioned(String version) { horizonVersion = version; return this; }"));
        assert!(source.contains("horizonPayloadCodecs"));
        assert!(source.contains("horizonPayloadVersions"));
    }

    #[test]
    fn fabric_payload_type_registry_retains_direction_payloads() {
        let source = include_str!("facade.rs");
        // The marker is a Fabric API library class: it is emitted only when
        // the real API jar is not on the target's classpath.
        assert!(source.contains("let uses_fabric_payloads = target.iter().any(|class| {"));
        assert!(
            source.contains("if uses_fabric_payloads && !available.contains(payload_registry) {")
        );
        assert!(source.contains("const FABRIC_PAYLOAD_TYPE_REGISTRY: &str"));
        for entry in [
            "public static <T> PayloadTypeRegistrar<T> playS2C() { return new PayloadTypeRegistrar<>(\"playToClient\"); }",
            "public static <T> PayloadTypeRegistrar<T> playC2S() { return new PayloadTypeRegistrar<>(\"playToServer\"); }",
            "public static <T> PayloadTypeRegistrar<T> configurationS2C() { return new PayloadTypeRegistrar<>(\"configurationToClient\"); }",
            "public static <T> PayloadTypeRegistrar<T> configurationC2S() { return new PayloadTypeRegistrar<>(\"configurationToServer\"); }",
            "public PayloadTypeRegistrar<T> register(Object type, Object codec) {",
            "public static java.util.List<Object> horizonPayloadIds(String direction) {",
            "public static java.util.List<Object> horizonPayloadCodecs(String direction) {",
        ] {
            assert!(source.contains(entry), "missing fragment: {entry}");
        }
        // No game class is required for the id lookup, so the shim stays
        // compilable against a jar-only target.
        assert!(!FABRIC_PAYLOAD_TYPE_REGISTRY.contains("net.minecraft."));
        assert!(NEOFORGE_PAYLOAD_REGISTRAR.contains("CustomPacketPayload.Type"));
        assert!(NEOFORGE_PAYLOAD_REGISTRAR.contains("StreamCodec"));
        assert!(FABRIC_PAYLOAD_TYPE_REGISTRY.contains("public final class PayloadTypeRegistry {"));
    }

    /// The payload registries are compiled and run for real: direction-keyed
    /// id retention is behaviour, not a source shape.
    #[test]
    fn payload_registries_compile_and_retain_ids() {
        use std::{path::PathBuf, process::Command};
        let probe = r#"public class PayloadProbe {
    public static final class Type {
        private final Object id;
        public Type(Object id) { this.id = id; }
        public Object id() { return id; }
    }
    public static void main(String[] args) {
        net.neoforged.neoforge.network.registration.PayloadRegistrar registrar = new net.neoforged.neoforge.network.registration.PayloadRegistrar();
        registrar.playToServer(new Type("mod:server"), null, null).playToClient(new Type("mod:client"), null, null);
        registrar.configurationToServer(new Type("mod:config"), null, null).optional().versioned("1").playBidirectional(new Type("mod:both"), null, null);
        if (registrar.horizonRegistrationCount() != 4) throw new AssertionError("count " + registrar.horizonRegistrationCount());
        if (!"mod:server".equals(net.neoforged.neoforge.network.registration.PayloadRegistrar.horizonPayloadIds("playToServer").get(0).toString())) throw new AssertionError("server id");
        if (!"mod:client".equals(net.neoforged.neoforge.network.registration.PayloadRegistrar.horizonPayloadIds("playToClient").get(0).toString())) throw new AssertionError("client id");
        if (!"mod:config".equals(net.neoforged.neoforge.network.registration.PayloadRegistrar.horizonPayloadIds("configurationToServer").get(0).toString())) throw new AssertionError("config id");
        if (!"mod:both".equals(net.neoforged.neoforge.network.registration.PayloadRegistrar.horizonPayloadIds("playBidirectional").get(0).toString())) throw new AssertionError("both id");
        if (net.neoforged.neoforge.network.registration.PayloadRegistrar.horizonPayloadCodecs("playToServer").get(0) != null) throw new AssertionError("server codec");
        if (!"1".equals(net.neoforged.neoforge.network.registration.PayloadRegistrar.horizonPayloadVersions("playBidirectional").get(0))) throw new AssertionError("version");
        if (!net.neoforged.neoforge.network.registration.PayloadRegistrar.horizonPayloadOptional("playBidirectional").get(0)) throw new AssertionError("optional");
        net.fabricmc.fabric.api.networking.v1.PayloadTypeRegistry.PayloadTypeRegistrar<Object> fabric = net.fabricmc.fabric.api.networking.v1.PayloadTypeRegistry.playS2C();
        Object codec = new Object();
        fabric.register(new Type("mod:s2c"), codec).register(new Type("mod:s2c2"), null);
        net.fabricmc.fabric.api.networking.v1.PayloadTypeRegistry.playC2S().register(new Type("mod:c2s"), null);
        net.fabricmc.fabric.api.networking.v1.PayloadTypeRegistry.configurationS2C().register(new Type("mod:cfg"), null);
        if (fabric.horizonRegistrationCount() != 2) throw new AssertionError("fabric count");
        if (!"playToClient".equals(fabric.horizonDirection())) throw new AssertionError("fabric direction");
        if (net.fabricmc.fabric.api.networking.v1.PayloadTypeRegistry.horizonPayloadIds("playToClient").size() != 2) throw new AssertionError("fabric ids");
        if (!"mod:c2s".equals(net.fabricmc.fabric.api.networking.v1.PayloadTypeRegistry.horizonPayloadIds("playToServer").get(0).toString())) throw new AssertionError("fabric c2s id");
        if (net.fabricmc.fabric.api.networking.v1.PayloadTypeRegistry.horizonPayloadIds("configurationToClient").size() != 1) throw new AssertionError("fabric config ids");
        if (net.fabricmc.fabric.api.networking.v1.PayloadTypeRegistry.horizonPayloadCodecs("playToClient").get(0) != codec) throw new AssertionError("fabric codec");
        System.out.println("horizon payload registries ok");
    }
}
"#;
        let root =
            std::env::temp_dir().join(format!("horizon-facade-payloads-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        let mut files = Vec::new();
        let mut write = |relative: &str, text: &str| {
            let file = src.join(relative);
            fs::create_dir_all(file.parent().expect("package directory")).expect("create package");
            fs::write(&file, text).expect("write java shim");
            files.push(file);
        };
        write(
            "net/neoforged/neoforge/network/registration/PayloadRegistrar.java",
            NEOFORGE_PAYLOAD_REGISTRAR,
        );
        write(
            "net/fabricmc/fabric/api/networking/v1/PayloadTypeRegistry.java",
            FABRIC_PAYLOAD_TYPE_REGISTRY,
        );
        for (path, source) in [
            (
                "net/minecraft/network/RegistryFriendlyByteBuf.java",
                "package net.minecraft.network; public class RegistryFriendlyByteBuf {}\n",
            ),
            (
                "net/minecraft/network/FriendlyByteBuf.java",
                "package net.minecraft.network; public class FriendlyByteBuf {}\n",
            ),
            (
                "net/minecraft/network/codec/StreamCodec.java",
                "package net.minecraft.network.codec; public class StreamCodec {}\n",
            ),
            (
                "net/minecraft/resources/Identifier.java",
                "package net.minecraft.resources; public class Identifier {}\n",
            ),
            (
                "net/minecraft/network/protocol/common/custom/CustomPacketPayload.java",
                "package net.minecraft.network.protocol.common.custom; public interface CustomPacketPayload { class Type {} }\n",
            ),
            (
                "net/neoforged/neoforge/network/handling/IPayloadContext.java",
                "package net.neoforged.neoforge.network.handling; public interface IPayloadContext {}\n",
            ),
            (
                "net/neoforged/neoforge/network/handling/IPayloadHandler.java",
                "package net.neoforged.neoforge.network.handling; public interface IPayloadHandler<T> { void handle(T payload, IPayloadContext context); }\n",
            ),
        ] {
            write(path, source);
        }
        write("PayloadProbe.java", probe);
        fs::create_dir_all(&classes).expect("create classes directory");
        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let javac = jdk_bin
            .as_ref()
            .map(|bin| bin.join("javac"))
            .unwrap_or_else(|| PathBuf::from("javac"));
        let Ok(javac_status) = Command::new(&javac)
            .arg("-nowarn")
            .arg("-d")
            .arg(&classes)
            .args(&files)
            .status()
        else {
            let _ = fs::remove_dir_all(&root);
            return;
        };
        assert!(
            javac_status.success(),
            "javac rejected the generated payload registries"
        );
        let java = jdk_bin
            .as_ref()
            .map(|bin| bin.join("java"))
            .unwrap_or_else(|| PathBuf::from("java"));
        let output = Command::new(&java)
            .arg("-cp")
            .arg(&classes)
            .arg("PayloadProbe")
            .output()
            .expect("run the payload probe");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "payload probe failed: {stderr}");
        assert!(
            stdout.contains("horizon payload registries ok"),
            "probe output: {stdout}{stderr}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn fabric_networking_facades_match_the_api_shapes() {
        let source = include_str!("facade.rs");
        // The slice is selected by the API classes a target names, and the game
        // types its signatures need are seeded because a mod that only calls
        // `registerGlobalReceiver` may never name `Type` itself.
        let detection = [
            "let uses_server_networking =",
            "        names_fabric(\"net/fabricmc/fabric/api/networking/v1/ServerPlayNetworking\");",
        ]
        .join("\n");
        assert!(source.contains(&detection));
        assert!(source.contains(
            &["if uses_client_networking", "!available.contains(client) {"].join(" && ")
        ));
        assert!(source.contains(
            "\"net/minecraft/network/protocol/common/custom/CustomPacketPayload$Type\","
        ));
        assert!(source.contains(
            "used.entry(\n            \"net/minecraft/network/protocol/common/custom/CustomPacketPayload$Type\".to_owned(),\n        )"
        ));
        // A real API class is never shadowed, and a shim whose game types are
        // absent is reported rather than invented.
        assert!(source.contains("if available.contains(class) {\n        return Ok(());\n    }"));
        assert!(source.contains("report\n            .missing\n            .push(format!(\"{class} (needs {})\", missing.join(\", \")));"));
        assert!(source.contains("None => report.missing.push(format!(\n"));
        // Server holder: exact signatures and the per-type receiver map.
        for fragment in [
            "public static boolean registerGlobalReceiver(net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type type, PlayPayloadHandler handler)",
            "public static boolean canSend(net.minecraft.server.level.ServerPlayer player, net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type type)",
            "public static void send(net.minecraft.server.level.ServerPlayer player, net.minecraft.network.protocol.common.custom.CustomPacketPayload payload)",
            "horizonRecord(\"playToClient\", payload);",
            "void receive(net.minecraft.network.protocol.common.custom.CustomPacketPayload payload, Context context);",
            "net.minecraft.server.level.ServerPlayer player();",
            "net.fabricmc.fabric.api.networking.v1.PacketSender responseSender();",
            "public interface PlayChannelHandler {",
        ] {
            assert!(
                FABRIC_SERVER_PLAY_NETWORKING.contains(fragment),
                "server shim missing: {fragment}"
            );
        }
        // Client holder: the real client package, both `canSend` forms the
        // façade can answer, and `sendToServer` as the requested alias.
        for fragment in [
            "public static boolean registerGlobalReceiver(net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type type, PlayPayloadHandler handler)",
            "public static boolean canSend(net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type type)",
            "public static boolean canSend(net.minecraft.client.multiplayer.ClientPacketListener listener, net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type type)",
            "public static void send(net.minecraft.network.protocol.common.custom.CustomPacketPayload payload)",
            "public static void sendToServer(net.minecraft.network.protocol.common.custom.CustomPacketPayload payload)",
            "horizonRecord(\"playToServer\", payload);",
            "{player} player();",
        ] {
            assert!(
                FABRIC_CLIENT_PLAY_NETWORKING.contains(fragment),
                "client shim missing: {fragment}"
            );
        }
        assert!(
            source.contains(
                "FABRIC_CLIENT_PLAY_NETWORKING.replace(\"{player}\", &java_name(player))"
            )
        );
        assert!(
            FABRIC_PACKET_SENDER
                .contains("void sendPacket(net.minecraft.network.protocol.common.custom.CustomPacketPayload payload);")
        );
    }

    /// Handler retention and the send markers are behaviour, so the two
    /// networking holders are compiled and run against bounded game stubs.
    #[test]
    fn fabric_networking_retains_handlers_and_records_sends() {
        use std::{path::PathBuf, process::Command};
        let probe = r#"public class NetworkingProbe {
    public static final class Payload implements net.minecraft.network.protocol.common.custom.CustomPacketPayload {
        private final Type horizonType;
        Payload(String id) { this.horizonType = new Type(new net.minecraft.resources.Identifier("mod", id)); }
        public Type type() { return horizonType; }
    }

    public static void main(String[] args) {
        net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type one =
            new net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type(new net.minecraft.resources.Identifier("mod", "one"));
        net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type two =
            new net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type(new net.minecraft.resources.Identifier("mod", "two"));
        net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type three =
            new net.minecraft.network.protocol.common.custom.CustomPacketPayload.Type(new net.minecraft.resources.Identifier("mod", "three"));
        final int[] calls = new int[2];
        boolean first = net.fabricmc.fabric.api.networking.v1.ServerPlayNetworking.registerGlobalReceiver(one, (payload, context) -> calls[0]++);
        boolean duplicate = net.fabricmc.fabric.api.networking.v1.ServerPlayNetworking.registerGlobalReceiver(one, (payload, context) -> calls[1]++);
        boolean second = net.fabricmc.fabric.api.networking.v1.ServerPlayNetworking.registerGlobalReceiver(two, (payload, context) -> calls[1] += 10);
        if (!first || duplicate || !second) throw new AssertionError("register flags");
        if (!net.fabricmc.fabric.api.networking.v1.ServerPlayNetworking.canSend(null, one)) throw new AssertionError("canSend one");
        if (!net.fabricmc.fabric.api.networking.v1.ServerPlayNetworking.canSend(null, two)) throw new AssertionError("canSend two");
        if (net.fabricmc.fabric.api.networking.v1.ServerPlayNetworking.canSend(null, three)) throw new AssertionError("canSend three");
        if (net.fabricmc.fabric.api.networking.v1.ServerPlayNetworking.horizonReceiver("mod:one") == null) throw new AssertionError("receiver one");
        if (net.fabricmc.fabric.api.networking.v1.ServerPlayNetworking.horizonReceiver("mod:two") == null) throw new AssertionError("receiver two");
        net.fabricmc.fabric.api.networking.v1.ServerPlayNetworking.horizonReceiver("mod:one").receive(new Payload("x"), null);
        net.fabricmc.fabric.api.networking.v1.ServerPlayNetworking.horizonReceiver("mod:two").receive(new Payload("x"), null);
        if (calls[0] != 1 || calls[1] != 10) throw new AssertionError("handler retention " + calls[0] + " " + calls[1]);
        net.fabricmc.fabric.api.networking.v1.ServerPlayNetworking.send(null, new Payload("serverSend"));
        java.util.List<String> serverSends = net.fabricmc.fabric.api.networking.v1.ServerPlayNetworking.horizonSends();
        if (serverSends.size() != 1 || !"playToClient mod:serverSend".equals(serverSends.get(0))) throw new AssertionError("server send " + serverSends);

        boolean clientFirst = net.fabricmc.fabric.api.client.networking.v1.ClientPlayNetworking.registerGlobalReceiver(one, (payload, context) -> calls[0]++);
        if (!clientFirst) throw new AssertionError("client register");
        if (!net.fabricmc.fabric.api.client.networking.v1.ClientPlayNetworking.canSend(one)) throw new AssertionError("client canSend");
        if (!net.fabricmc.fabric.api.client.networking.v1.ClientPlayNetworking.canSend((net.minecraft.client.multiplayer.ClientPacketListener) null, one)) throw new AssertionError("client listener canSend");
        if (net.fabricmc.fabric.api.client.networking.v1.ClientPlayNetworking.canSend(three)) throw new AssertionError("client canSend three");
        net.fabricmc.fabric.api.client.networking.v1.ClientPlayNetworking.send(new Payload("clientSend"));
        net.fabricmc.fabric.api.client.networking.v1.ClientPlayNetworking.sendToServer(new Payload("clientSend2"));
        java.util.List<String> clientSends = net.fabricmc.fabric.api.client.networking.v1.ClientPlayNetworking.horizonSends();
        if (clientSends.size() != 2 || !"playToServer mod:clientSend".equals(clientSends.get(0)) || !"playToServer mod:clientSend2".equals(clientSends.get(1))) throw new AssertionError("client sends " + clientSends);
        System.out.println("horizon fabric networking ok");
    }
}
"#;
        let root =
            std::env::temp_dir().join(format!("horizon-facade-networking-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        let mut files = Vec::new();
        let mut write = |relative: &str, text: &str| {
            let file = src.join(relative);
            fs::create_dir_all(file.parent().expect("package directory")).expect("create package");
            fs::write(&file, text).expect("write java shim");
            files.push(file);
        };
        write(
            "net/minecraft/resources/Identifier.java",
            "package net.minecraft.resources;\npublic class Identifier {\n    private final String horizonNamespace;\n    private final String horizonPath;\n    public Identifier(String namespace, String path) { this.horizonNamespace = namespace; this.horizonPath = path; }\n    public String toString() { return horizonNamespace + \":\" + horizonPath; }\n}\n",
        );
        write(
            "net/minecraft/network/protocol/common/custom/CustomPacketPayload.java",
            "package net.minecraft.network.protocol.common.custom;\npublic interface CustomPacketPayload {\n    Type type();\n    final class Type {\n        private final net.minecraft.resources.Identifier horizonId;\n        public Type(net.minecraft.resources.Identifier id) { this.horizonId = id; }\n        public net.minecraft.resources.Identifier id() { return horizonId; }\n        public String toString() { return horizonId.toString(); }\n    }\n}\n",
        );
        write(
            "net/minecraft/server/level/ServerPlayer.java",
            "package net.minecraft.server.level;\npublic class ServerPlayer {}\n",
        );
        write(
            "net/minecraft/client/multiplayer/ClientPacketListener.java",
            "package net.minecraft.client.multiplayer;\npublic class ClientPacketListener {}\n",
        );
        write(
            "net/minecraft/client/player/LocalPlayer.java",
            "package net.minecraft.client.player;\npublic class LocalPlayer {}\n",
        );
        write(
            "net/fabricmc/fabric/api/networking/v1/PacketSender.java",
            FABRIC_PACKET_SENDER,
        );
        write(
            "net/fabricmc/fabric/api/networking/v1/ServerPlayNetworking.java",
            FABRIC_SERVER_PLAY_NETWORKING,
        );
        write(
            "net/fabricmc/fabric/api/client/networking/v1/ClientPlayNetworking.java",
            &FABRIC_CLIENT_PLAY_NETWORKING
                .replace("{player}", "net.minecraft.client.player.LocalPlayer"),
        );
        write("NetworkingProbe.java", probe);
        fs::create_dir_all(&classes).expect("create classes directory");
        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let javac = jdk_bin
            .as_ref()
            .map(|bin| bin.join("javac"))
            .unwrap_or_else(|| PathBuf::from("javac"));
        let Ok(javac_status) = Command::new(&javac)
            .arg("-nowarn")
            .arg("-d")
            .arg(&classes)
            .args(&files)
            .status()
        else {
            let _ = fs::remove_dir_all(&root);
            return;
        };
        assert!(
            javac_status.success(),
            "javac rejected the generated networking façade"
        );
        let java = jdk_bin
            .as_ref()
            .map(|bin| bin.join("java"))
            .unwrap_or_else(|| PathBuf::from("java"));
        let output = Command::new(&java)
            .arg("-cp")
            .arg(&classes)
            .arg("NetworkingProbe")
            .output()
            .expect("run the networking probe");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "networking probe failed: {stderr}");
        assert!(
            stdout.contains("horizon fabric networking ok"),
            "probe output: {stdout}{stderr}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn fabric_event_holders_match_the_api_descriptors() {
        let server = fabric_event_source(
            "net.fabricmc.fabric.api.event.lifecycle.v1",
            "ServerLifecycleEvents",
            "doc",
            FABRIC_SERVER_LIFECYCLE,
        );
        for (field, callback, method) in [
            ("SERVER_STARTING", "ServerStarting", "onServerStarting"),
            ("SERVER_STARTED", "ServerStarted", "onServerStarted"),
            ("SERVER_STOPPING", "ServerStopping", "onServerStopping"),
        ] {
            assert!(
                server.contains(&format!(
                    "public static final Event<{callback}> {field} = Event.horizon(horizonListeners ->"
                )),
                "missing field {field}"
            );
            assert!(
                server.contains(&format!(
                    "void {method}(net.minecraft.server.MinecraftServer p0);"
                )),
                "missing descriptor {method}"
            );
            assert!(
                server.contains(&format!(
                    "for ({callback} horizonListener : horizonListeners) horizonListener.{method}(p0);"
                )),
                "missing invoker body {method}"
            );
        }
        let client = fabric_event_source(
            "net.fabricmc.fabric.api.event.lifecycle.v1",
            "ClientLifecycleEvents",
            "doc",
            FABRIC_CLIENT_LIFECYCLE,
        );
        assert!(client.contains("void onClientStarted(net.minecraft.client.Minecraft p0);"));
        assert!(client.contains("void onClientStopping(net.minecraft.client.Minecraft p0);"));
        let player = fabric_event_source(
            "net.fabricmc.fabric.api.event.player",
            "ServerPlayerEvents",
            "doc",
            FABRIC_SERVER_PLAYER_EVENTS,
        );
        assert!(player.contains(
            "void afterRespawn(net.minecraft.server.level.ServerPlayer p0, net.minecraft.server.level.ServerPlayer p1, boolean p2);"
        ));
        assert!(player.contains(
            "for (CopyFrom horizonListener : horizonListeners) horizonListener.copyFrom(p0, p1, p2);"
        ));
        // Registration order is the only order: the invoker walks the list.
        assert!(FABRIC_EVENT.contains("private final java.util.List<T> horizonListeners"));
        assert!(FABRIC_EVENT.contains("horizonListeners.add(listener);"));
        assert!(FABRIC_EVENT.contains("T invoker();"));
        // The command callback is functional and carries `EVENT`.
        assert!(FABRIC_COMMAND_REGISTRATION_CALLBACK.contains("@FunctionalInterface"));
        assert!(FABRIC_COMMAND_REGISTRATION_CALLBACK.contains(
            "void register(CommandDispatcher<CommandSourceStack> dispatcher, CommandBuildContext registryAccess, Commands.CommandSelection environment);"
        ));
        assert!(FABRIC_COMMAND_REGISTRATION_CALLBACK.contains(
            "Event<CommandRegistrationCallback> EVENT = Event.horizon(horizonListeners ->"
        ));
        assert!(!FABRIC_COMMAND_REGISTRATION_CALLBACK.contains("extends Event<"));
    }

    #[test]
    fn fabric_event_shims_only_fill_an_absent_api_class() {
        let source = include_str!("facade.rs");
        assert!(source.contains("let uses_fabric_events = target"));
        assert!(source.contains("let base_absent = !available.contains(event_base);"));
        assert!(source.contains("if base_absent {"));
        assert!(source.contains("if available.contains(class) {"));
        assert!(source.contains("let compilable =\n            |ty: &str| wanted.contains(ty)"));
        // A client-only callback is skipped on a server-shaped jar.
        assert!(source.contains("&[\"net/minecraft/client/Minecraft\"][..]"));
        assert!(source.contains(
            "if game.contains_key(name) && !lib_names.contains(name) && !own.contains(name) {"
        ));
        // The command callback needs the real dispatcher and the nested enum.
        assert!(source.contains("lib_names.contains(\"com/mojang/brigadier/CommandDispatcher\")"));
        assert!(
            source
                .contains("wanted.contains(\"net/minecraft/commands/Commands$CommandSelection\")")
        );
        // The loader slice is all-or-nothing: no mixed real/bounded shapes.
        assert!(source.contains("FABRIC_LOADER_SHIMS\n            .iter()\n            .all("));
        for (class, _) in FABRIC_LOADER_SHIMS {
            assert!(
                source.contains(&format!("\"{class}\"")),
                "loader shim not registered: {class}"
            );
        }
    }

    #[test]
    fn neoforge_event_bus_surface_covers_result_cancel_and_priority() {
        for fragment in [
            "public enum Result { DENY, DEFAULT, ALLOW }",
            "public Result getResult()",
            "public void setResult(Result result)",
            "public boolean hasResult()",
            "public boolean isCanceled()",
            "public void setCanceled(boolean canceled)",
            "public boolean isCancelable()",
        ] {
            assert!(NEOFORGE_EVENT.contains(fragment), "missing: {fragment}");
        }
        assert!(FORGE_EVENT.contains("public Result getResult()"));
        assert!(FORGE_EVENT.contains("public boolean hasResult()"));
        for fragment in [
            "public @interface SubscribeEvent {",
            "EventPriority priority() default EventPriority.NORMAL;",
            "boolean receiveCancelled() default false;",
            "@Target(ElementType.METHOD)",
        ] {
            assert!(
                NEOFORGE_SUBSCRIBE_EVENT.contains(fragment),
                "missing: {fragment}"
            );
        }
        let source = include_str!("facade.rs");
        assert!(source.contains("\"net/neoforged/bus/api/SubscribeEvent.java\","));
        for overload in [
            "default IEventBus addListener(EventPriority priority, Consumer<?> listener) { return this; }",
            "default IEventBus addListener(EventPriority priority, boolean receiveCancelled, Consumer<?> listener) { return this; }",
            "default <T extends Event> IEventBus addListener(Class<T> type, Consumer<T> listener) { return this; }",
            "default <T extends Event> IEventBus addListener(EventPriority priority, Class<T> type, Consumer<T> listener) { return this; }",
            "default <T extends Event> IEventBus addListener(EventPriority priority, boolean receiveCancelled, Class<T> type, Consumer<T> listener) { return this; }",
        ] {
            assert!(source.contains(overload), "missing overload: {overload}");
        }
        assert!(
            source.contains("public enum EventPriority { HIGHEST, HIGH, NORMAL, LOW, LOWEST }")
        );
    }

    #[test]
    fn fabric_loader_bootstrap_values_are_bounded() {
        for fragment in [
            "public static FabricLoader getInstance()",
            "public <T> java.util.List<EntrypointContainer<T>> getEntrypointContainers(String key, Class<T> type)",
            "public java.util.Optional<ModContainer> getModContainer(String modId)",
            "public java.util.Collection<ModContainer> getAllMods()",
            "public ObjectShare getObjectShare()",
            "public boolean isModLoaded(String modId)",
        ] {
            assert!(FABRIC_LOADER.contains(fragment), "missing: {fragment}");
        }
        assert!(FABRIC_MOD_CONTAINER.contains("ModMetadata getMetadata();"));
        assert!(FABRIC_MOD_CONTAINER.contains("default Version getVersion()"));
        assert!(FABRIC_MOD_METADATA.contains("Version getVersion();"));
        assert!(FABRIC_VERSION.contains("String getFriendlyString();"));
        for fragment in [
            "public Object get(String key)",
            "public <T> T get(Class<T> type, String key)",
            "public <T> T getOrDefault(String key, T fallback)",
            "public Object put(String key, Object value)",
        ] {
            assert!(
                FABRIC_OBJECT_SHARE.contains(fragment),
                "missing: {fragment}"
            );
        }
        // Bounded markers must not pull a game class into the loader slice: it
        // has to link on a jar-only classpath.
        for source in [
            FABRIC_LOADER,
            FABRIC_MOD_CONTAINER,
            FABRIC_ENTRYPOINT_CONTAINER,
            FABRIC_OBJECT_SHARE,
            FABRIC_MOD_METADATA,
            FABRIC_VERSION,
        ] {
            assert!(!source.contains("net.minecraft."));
        }
    }

    /// The generated event holders are compiled and run for real: registration
    /// order and the NeoForge priority contract are behaviour, not a shape.
    /// When `javac` is unavailable the source assertions above still hold.
    #[test]
    fn bounded_event_dispatch_compiles_and_keeps_order() {
        let server = fabric_event_source(
            "net.fabricmc.fabric.api.event.lifecycle.v1",
            "ServerLifecycleEvents",
            "Bounded server lifecycle events.",
            FABRIC_SERVER_LIFECYCLE,
        );
        let client = fabric_event_source(
            "net.fabricmc.fabric.api.event.lifecycle.v1",
            "ClientLifecycleEvents",
            "Bounded client lifecycle events.",
            FABRIC_CLIENT_LIFECYCLE,
        );
        let player = fabric_event_source(
            "net.fabricmc.fabric.api.event.player",
            "ServerPlayerEvents",
            "Bounded server player events.",
            FABRIC_SERVER_PLAYER_EVENTS,
        );
        let probe = r#"public class BootstrapProbe {
    public static final class ExampleEvent extends net.neoforged.bus.api.Event {
        public ExampleEvent() { super(true); }
    }

    public static final class Listener {
        @net.neoforged.bus.api.SubscribeEvent(priority = net.neoforged.bus.api.EventPriority.HIGH, receiveCancelled = true)
        public void on(ExampleEvent event) {}
    }

    public static void main(String[] args) throws Exception {
        java.util.List<String> seen = new java.util.ArrayList<>();
        net.fabricmc.fabric.api.event.lifecycle.v1.ServerLifecycleEvents.SERVER_STARTING.register(server -> seen.add("first"));
        net.fabricmc.fabric.api.event.lifecycle.v1.ServerLifecycleEvents.SERVER_STARTING.register(server -> seen.add("second"));
        net.fabricmc.fabric.api.event.lifecycle.v1.ServerLifecycleEvents.ServerStarting invoker =
            net.fabricmc.fabric.api.event.lifecycle.v1.ServerLifecycleEvents.SERVER_STARTING.invoker();
        invoker.onServerStarting(null);
        if (!seen.equals(java.util.Arrays.asList("first", "second"))) throw new AssertionError("order " + seen);

        net.neoforged.bus.api.EventPriority[] byPriority = {
            net.neoforged.bus.api.EventPriority.HIGHEST,
            net.neoforged.bus.api.EventPriority.HIGH,
            net.neoforged.bus.api.EventPriority.NORMAL,
            net.neoforged.bus.api.EventPriority.LOW,
            net.neoforged.bus.api.EventPriority.LOWEST,
        };
        for (int i = 1; i < byPriority.length; i++) {
            if (byPriority[i - 1].ordinal() >= byPriority[i].ordinal()) throw new AssertionError("priority order");
        }
        ExampleEvent event = new ExampleEvent();
        if (event.hasResult()) throw new AssertionError("default has no result");
        event.setResult(net.neoforged.bus.api.Event.Result.DENY);
        if (!event.hasResult() || event.getResult() != net.neoforged.bus.api.Event.Result.DENY) throw new AssertionError("result");
        event.setCanceled(true);
        if (!event.isCanceled() || !event.isCancelable()) throw new AssertionError("cancel");

        net.neoforged.bus.api.SubscribeEvent metadata = Listener.class
            .getMethod("on", ExampleEvent.class)
            .getAnnotation(net.neoforged.bus.api.SubscribeEvent.class);
        if (metadata == null
            || metadata.priority() != net.neoforged.bus.api.EventPriority.HIGH
            || !metadata.receiveCancelled()) throw new AssertionError("metadata");

        net.neoforged.bus.api.IEventBus bus = new net.neoforged.bus.api.IEventBus() {};
        bus.addListener(ExampleEvent.class, e -> {})
            .addListener(net.neoforged.bus.api.EventPriority.HIGH, ExampleEvent.class, e -> {})
            .addListener(net.neoforged.bus.api.EventPriority.LOW, true, ExampleEvent.class, e -> {})
            .register(new Listener());
        if (bus.post(event)) throw new AssertionError("bounded post");

        net.fabricmc.loader.api.FabricLoader loader = net.fabricmc.loader.api.FabricLoader.getInstance();
        if (!loader.isModLoaded("minecraft") || loader.isModLoaded("absent")) throw new AssertionError("isModLoaded");
        net.fabricmc.loader.api.ModContainer container = loader.getModContainer("minecraft").orElseThrow();
        if (!"minecraft".equals(container.getMetadata().getId())) throw new AssertionError("metadata id");
        if (!"26.3".equals(container.getMetadata().getVersion().getFriendlyString())) throw new AssertionError("version");
        if (loader.getAllMods().size() != 1) throw new AssertionError("mods");
        net.fabricmc.loader.api.ObjectShare share = loader.getObjectShare();
        share.put("key", "value");
        if (!"value".equals(share.getOrDefault("key", "fallback"))) throw new AssertionError("share");
        if (!"fallback".equals(share.getOrDefault("absent", "fallback"))) throw new AssertionError("share fallback");
        if (!loader.getEntrypointContainers("main", Object.class).isEmpty()) throw new AssertionError("entrypoints");
        System.out.println("horizon bootstrap facade ok");
    }
}
"#;
        let root =
            std::env::temp_dir().join(format!("horizon-facade-bootstrap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        let mut files = Vec::new();
        let mut write = |relative: &str, text: &str| {
            let file = src.join(relative);
            fs::create_dir_all(file.parent().expect("package directory")).expect("create package");
            fs::write(&file, text).expect("write java shim");
            files.push(file);
        };
        write("net/fabricmc/fabric/api/event/Event.java", FABRIC_EVENT);
        write(
            "net/fabricmc/fabric/api/event/lifecycle/v1/ServerLifecycleEvents.java",
            &server,
        );
        write(
            "net/fabricmc/fabric/api/event/lifecycle/v1/ClientLifecycleEvents.java",
            &client,
        );
        write(
            "net/fabricmc/fabric/api/event/player/ServerPlayerEvents.java",
            &player,
        );
        // The smallest game types the callback signatures name.
        write(
            "net/minecraft/server/MinecraftServer.java",
            "package net.minecraft.server;\npublic class MinecraftServer {}\n",
        );
        write(
            "net/minecraft/client/Minecraft.java",
            "package net.minecraft.client;\npublic class Minecraft {}\n",
        );
        write(
            "net/minecraft/server/level/ServerPlayer.java",
            "package net.minecraft.server.level;\npublic class ServerPlayer {}\n",
        );
        write("net/neoforged/bus/api/Event.java", NEOFORGE_EVENT);
        write(
            "net/neoforged/bus/api/SubscribeEvent.java",
            NEOFORGE_SUBSCRIBE_EVENT,
        );
        write(
            "net/neoforged/bus/api/EventPriority.java",
            "package net.neoforged.bus.api;\npublic enum EventPriority { HIGHEST, HIGH, NORMAL, LOW, LOWEST }\n",
        );
        write(
            "net/neoforged/bus/api/IEventBus.java",
            "package net.neoforged.bus.api;\nimport java.util.function.Consumer;\npublic interface IEventBus {\n    default IEventBus addListener(Consumer<?> listener) { return this; }\n    default IEventBus addListener(EventPriority priority, Consumer<?> listener) { return this; }\n    default <T extends Event> IEventBus addListener(Class<T> type, Consumer<T> listener) { return this; }\n    default <T extends Event> IEventBus addListener(EventPriority priority, Class<T> type, Consumer<T> listener) { return this; }\n    default <T extends Event> IEventBus addListener(EventPriority priority, boolean receiveCancelled, Class<T> type, Consumer<T> listener) { return this; }\n    default IEventBus register(Object object) { return this; }\n    default IEventBus unregister(Object object) { return this; }\n    default boolean post(Event event) { return false; }\n}\n",
        );
        for (class, source) in FABRIC_LOADER_SHIMS {
            write(&format!("{class}.java"), source);
        }
        write("BootstrapProbe.java", probe);
        fs::create_dir_all(&classes).expect("create classes directory");
        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let javac = jdk_bin
            .as_ref()
            .map(|bin| bin.join("javac"))
            .unwrap_or_else(|| PathBuf::from("javac"));
        let Ok(javac_status) = Command::new(&javac)
            .arg("-nowarn")
            .arg("-d")
            .arg(&classes)
            .args(&files)
            .status()
        else {
            let _ = fs::remove_dir_all(&root);
            return;
        };
        assert!(
            javac_status.success(),
            "javac rejected the generated bootstrap façade"
        );
        let java = jdk_bin
            .as_ref()
            .map(|bin| bin.join("java"))
            .unwrap_or_else(|| PathBuf::from("java"));
        let output = Command::new(&java)
            .arg("-cp")
            .arg(&classes)
            .arg("BootstrapProbe")
            .output()
            .expect("run the bootstrap probe");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "bootstrap probe failed: {stderr}");
        assert!(
            stdout.contains("horizon bootstrap facade ok"),
            "probe output: {stdout}{stderr}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn neoforge_capability_values_keep_lazy_optional_shape() {
        assert_eq!(
            runtime_body(
                "net/neoforged/neoforge/capabilities/CapabilityManager",
                "get",
                "net.neoforged.neoforge.capabilities.Capability"
            ),
            "return new net.neoforged.neoforge.capabilities.Capability();"
        );
        assert_eq!(
            runtime_body(
                "net/minecraftforge/common/util/LazyOptional",
                "isPresent",
                "boolean"
            ),
            "return horizonValid && horizonGet() != null;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraftforge/common/util/LazyOptional",
                "invalidate",
                "void"
            ),
            "horizonValid = false; horizonValue = null; horizonSupplier = () -> null;"
        );
        assert_eq!(
            runtime_body(
                "net/neoforged/neoforge/capabilities/Capability",
                "isRegistered",
                "boolean"
            ),
            "return true;"
        );
        assert_eq!(
            runtime_body(
                "net/neoforged/neoforge/common/util/LazyOptional",
                "isPresent",
                "boolean"
            ),
            "return horizonValid && horizonGet() != null;"
        );
        assert_eq!(
            runtime_body(
                "net/neoforged/neoforge/common/util/LazyOptional",
                "invalidate",
                "void"
            ),
            "horizonValid = false; horizonValue = null; horizonSupplier = () -> null;"
        );
        assert_eq!(
            runtime_body(
                "net/neoforged/neoforge/capabilities/Capability",
                "orEmpty",
                "net.neoforged.neoforge.common.util.LazyOptional"
            ),
            "return p1 == this && p0 != null ? net.neoforged.neoforge.common.util.LazyOptional.ofNullable(p0) : net.neoforged.neoforge.common.util.LazyOptional.empty();"
        );
        assert_eq!(
            runtime_body(
                "net/minecraftforge/common/capabilities/CapabilityManager",
                "get",
                "net.minecraftforge.common.capabilities.Capability"
            ),
            "return new net.minecraftforge.common.capabilities.Capability();"
        );
        assert_eq!(
            runtime_body(
                "net/minecraftforge/common/capabilities/Capability",
                "orEmpty",
                "net.minecraftforge.common.util.LazyOptional"
            ),
            "return p1 == this && p0 != null ? net.minecraftforge.common.util.LazyOptional.ofNullable(p0) : net.minecraftforge.common.util.LazyOptional.empty();"
        );
    }

    #[test]
    fn legacy_forge_event_and_capability_aliases_are_emitted() {
        let source = include_str!("facade.rs");
        assert!(source.contains("let uses_legacy_forge = target.iter().any"));
        assert!(source.contains("net/minecraftforge/eventbus/api/SubscribeEvent.java"));
        assert!(source.contains("public static final IEventBus EVENT_BUS"));
        assert!(source.contains("net/minecraftforge/fml/javafmlmod/FMLJavaModLoadingContext.java"));
        assert!(source.contains("net/minecraftforge/common/capabilities/ICapabilityProvider.java"));
        assert_eq!(
            runtime_body(
                "net/minecraftforge/eventbus/api/IEventBus",
                "register",
                "net.minecraftforge.eventbus.api.IEventBus"
            ),
            "return this;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraftforge/eventbus/api/IEventBus",
                "post",
                "boolean"
            ),
            "return false;"
        );
    }

    #[test]
    fn neoforge_item_handler_contract_is_bounded_and_stateful() {
        // The source shim is intentionally kept beside the façade generator,
        // so assert the ABI policy here: one bounded slot, simulation support,
        // and a mutable counterpart. This guards against silently regressing
        // to shape-only abstract methods when the shim evolves.
        let source = include_str!("facade.rs");
        assert!(source.contains("default int getSlots() { return 1; }"));
        assert!(source.contains("Math.min(incoming, Math.max(0, 64 - value.count))"));
        assert!(source.contains("if (!simulate && amount > 0)"));
        assert!(source.contains("if (!simulate) { value.count -= moved;"));
        assert!(source.contains("copyWithCount\\\", int.class"));
        assert!(source.contains("default void setStackInSlot(int slot, ItemStack stack)"));
        assert!(source.contains("Collections.synchronizedMap(new java.util.WeakHashMap<>)"));
        assert!(source.contains("if class.name == \"net/minecraft/world/item/ItemStack\""));
        assert!(source.contains("(\"getCount\", \"()I\", \"int\", \"\")"));
    }

    /// Every `FriendlyByteBuf` pair the façade encodes for real. The table
    /// drives both the body assertions and the Java compile probe, so a pair
    /// that silently falls back to the throwing stub fails the suite.
    const FRIENDLY_BYTE_BUF_PAIRS: &[(&str, &str)] = &[
        ("writeByte", "(I)Lnet/minecraft/network/FriendlyByteBuf;"),
        ("readByte", "()B"),
        ("readUnsignedByte", "()S"),
        ("writeShort", "(I)Lnet/minecraft/network/FriendlyByteBuf;"),
        ("readShort", "()S"),
        ("readUnsignedShort", "()I"),
        ("writeShortLE", "(I)Lnet/minecraft/network/FriendlyByteBuf;"),
        ("readShortLE", "()S"),
        ("readUnsignedShortLE", "()I"),
        ("writeMedium", "(I)Lnet/minecraft/network/FriendlyByteBuf;"),
        ("readMedium", "()I"),
        (
            "writeMediumLE",
            "(I)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        ("readMediumLE", "()I"),
        ("writeChar", "(C)Lnet/minecraft/network/FriendlyByteBuf;"),
        ("readChar", "()C"),
        ("writeInt", "(I)Lnet/minecraft/network/FriendlyByteBuf;"),
        ("readInt", "()I"),
        ("writeIntLE", "(I)Lnet/minecraft/network/FriendlyByteBuf;"),
        ("readIntLE", "()I"),
        ("writeVarInt", "(I)Lnet/minecraft/network/FriendlyByteBuf;"),
        ("readVarInt", "()I"),
        ("writeLong", "(J)Lnet/minecraft/network/FriendlyByteBuf;"),
        ("readLong", "()J"),
        ("writeLongLE", "(J)Lnet/minecraft/network/FriendlyByteBuf;"),
        ("readLongLE", "()J"),
        ("writeVarLong", "(J)Lnet/minecraft/network/FriendlyByteBuf;"),
        ("readVarLong", "()J"),
        ("writeFloat", "(F)Lnet/minecraft/network/FriendlyByteBuf;"),
        ("readFloat", "()F"),
        ("writeDouble", "(D)Lnet/minecraft/network/FriendlyByteBuf;"),
        ("readDouble", "()D"),
        ("writeBoolean", "(Z)Lnet/minecraft/network/FriendlyByteBuf;"),
        ("readBoolean", "()Z"),
        (
            "writeUtf",
            "(Ljava/lang/String;)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        (
            "writeUtf",
            "(Ljava/lang/String;I)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        ("readUtf", "()Ljava/lang/String;"),
        ("readUtf", "(I)Ljava/lang/String;"),
        ("writeString", "(Ljava/lang/String;)V"),
        (
            "writeString",
            "(Ljava/lang/String;)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        ("readString", "()Ljava/lang/String;"),
        (
            "writeResourceLocation",
            "(Lnet/minecraft/resources/Identifier;)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        (
            "readResourceLocation",
            "()Lnet/minecraft/resources/Identifier;",
        ),
        (
            "writeIdentifier",
            "(Lnet/minecraft/resources/Identifier;)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        ("readIdentifier", "()Lnet/minecraft/resources/Identifier;"),
        (
            "writeUUID",
            "(Ljava/util/UUID;)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        ("readUUID", "()Ljava/util/UUID;"),
        (
            "writeBlockPos",
            "(Lnet/minecraft/core/BlockPos;)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        ("readBlockPos", "()Lnet/minecraft/core/BlockPos;"),
        (
            "writeChunkPos",
            "(Lnet/minecraft/core/ChunkPos;)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        ("readChunkPos", "()Lnet/minecraft/core/ChunkPos;"),
        (
            "writeByteArray",
            "([B)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        ("readByteArray", "()[B"),
        ("readByteArray", "(I)[B"),
        ("writeBytes", "([B)Lnet/minecraft/network/FriendlyByteBuf;"),
        ("writeBooleanArray", "([Z)V"),
        (
            "writeBooleanArray",
            "([Z)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        ("readBooleanArray", "()[Z"),
        ("writeVarIntArray", "([I)V"),
        (
            "writeVarIntArray",
            "([I)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        ("readVarIntArray", "()[I"),
        // The old spelling stays an alias of the same VarInt wire form.
        ("writeIntArray", "([I)V"),
        (
            "writeIntArray",
            "([I)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        ("readIntArray", "()[I"),
        ("writeLongArray", "([J)V"),
        (
            "writeLongArray",
            "([J)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        ("readLongArray", "()[J"),
        ("writeFloatArray", "([F)V"),
        (
            "writeFloatArray",
            "([F)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        ("readFloatArray", "()[F"),
        ("writeDoubleArray", "([D)V"),
        (
            "writeDoubleArray",
            "([D)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        ("readDoubleArray", "()[D"),
        ("writeStringArray", "([Ljava/lang/String;)V"),
        (
            "writeStringArray",
            "([Ljava/lang/String;)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        ("readStringArray", "()[Ljava/lang/String;"),
        ("writeFloatLE", "(F)V"),
        ("writeFloatLE", "(F)Lnet/minecraft/network/FriendlyByteBuf;"),
        ("readFloatLE", "()F"),
        ("writeDoubleLE", "(D)V"),
        (
            "writeDoubleLE",
            "(D)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        ("readDoubleLE", "()D"),
        (
            "writeEnum",
            "(Ljava/lang/Enum;)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        ("readEnum", "(Ljava/lang/Class;)Ljava/lang/Enum;"),
        (
            "writeNullable",
            "(Ljava/lang/Object;Ljava/util/function/BiConsumer;)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        (
            "readNullable",
            "(Ljava/util/function/Function;)Ljava/lang/Object;",
        ),
        (
            "writeOptional",
            "(Ljava/util/Optional;Ljava/util/function/BiConsumer;)Lnet/minecraft/network/FriendlyByteBuf;",
        ),
        (
            "readOptional",
            "(Ljava/util/function/Function;)Ljava/util/Optional;",
        ),
        ("readerIndex", "()I"),
        ("readerIndex", "(I)Lnet/minecraft/network/FriendlyByteBuf;"),
        ("readerIndex", "(I)I"),
        ("writerIndex", "()I"),
        ("writerIndex", "(I)Lnet/minecraft/network/FriendlyByteBuf;"),
        ("writerIndex", "(I)I"),
        ("setIndex", "(I)V"),
        ("readableBytes", "()I"),
        ("remaining", "()I"),
        ("isReadable", "()Z"),
        ("isReadable", "(I)Z"),
        ("skipBytes", "(I)Lnet/minecraft/network/FriendlyByteBuf;"),
        ("clear", "()Lnet/minecraft/network/FriendlyByteBuf;"),
        (
            "discardReadBytes",
            "()Lnet/minecraft/network/FriendlyByteBuf;",
        ),
    ];

    fn friendly_byte_buf_body(method: &str, descriptor: &str) -> String {
        let (ret, _) = signature(descriptor);
        runtime_body_with_descriptor(
            "net/minecraft/network/FriendlyByteBuf",
            method,
            &ret,
            descriptor,
        )
    }

    #[test]
    fn friendly_byte_buf_state_is_bounded_and_wire_backed() {
        let source = include_str!("facade.rs");
        assert!(source.contains("protected java.util.List<Object> horizonValues"));
        assert!(source.contains("protected byte[] horizonBytes = new byte[0];"));
        assert!(source.contains("protected int horizonWriteIndex;"));
        assert!(source.contains("protected int horizonByteReadIndex;"));
        assert!(source.contains("protected static final int HORIZON_MAX_BYTES = 1048576;"));
        for helper in [
            "protected void horizonEnsure(int extra)",
            "protected void horizonPutBytes(byte[] value)",
            "protected void horizonPutVarInt(int value)",
            "protected long horizonGetVarLong()",
            "protected void horizonPutUtf(String value, int maxLength)",
            "protected String horizonGetUtf(int maxLength)",
            "protected byte[] horizonGetByteArray(int maxLength)",
            "protected void horizonCompact()",
            "protected void horizonSetWriterIndex(int index)",
        ] {
            assert!(source.contains(helper), "missing helper: {helper}");
        }
        // The state block is injected into the generated buffer, and a
        // Netty-backed construction copies its readable window in.
        assert!(source.contains("write!(out, \"{FRIENDLY_BYTE_BUF_STATE}\");"));
        assert!(source.contains("\"(Lio/netty/buffer/ByteBuf;)V\""));
        assert!(source.contains("p0.readBytes(bytes)"));
    }

    #[test]
    fn friendly_byte_buf_bodies_encode_and_decode() {
        for (method, descriptor) in FRIENDLY_BYTE_BUF_PAIRS {
            let body = friendly_byte_buf_body(method, descriptor);
            assert!(
                !body.contains("UnsupportedOperationException"),
                "{method}{descriptor} has no body: {body}"
            );
            // `clear` empties both cursors, which is the one legitimate
            // mention of the opaque marker FIFO in a wire pair.
            assert!(
                *method == "clear" || !body.contains("horizonValues"),
                "{method}{descriptor} still uses the marker FIFO: {body}"
            );
        }
        assert_eq!(
            friendly_byte_buf_body("writeVarInt", "(I)Lnet/minecraft/network/FriendlyByteBuf;"),
            "horizonPutVarInt(p0); return this;"
        );
        assert_eq!(
            friendly_byte_buf_body("readInt", "()I"),
            "return horizonGetInt();"
        );
        assert_eq!(
            friendly_byte_buf_body("readUtf", "(I)Ljava/lang/String;"),
            "return horizonGetUtf(p0);"
        );
        assert_eq!(
            friendly_byte_buf_body(
                "writeUtf",
                "(Ljava/lang/String;I)Lnet/minecraft/network/FriendlyByteBuf;"
            ),
            "horizonPutUtf(p0, p1); return this;"
        );
        assert_eq!(
            friendly_byte_buf_body("writeString", "(Ljava/lang/String;)V"),
            "horizonPutUtf(p0, 32767);"
        );
        assert_eq!(
            friendly_byte_buf_body("readString", "()Ljava/lang/String;"),
            "return horizonGetUtf(32767);"
        );
        assert_eq!(
            friendly_byte_buf_body(
                "writeString",
                "(Ljava/lang/String;)Lnet/minecraft/network/FriendlyByteBuf;"
            ),
            "horizonPutUtf(p0, 32767); return this;"
        );
        assert_eq!(
            friendly_byte_buf_body(
                "writeDoubleLE",
                "(D)Lnet/minecraft/network/FriendlyByteBuf;"
            ),
            "horizonPutLongLE(Double.doubleToRawLongBits(p0)); return this;"
        );
        assert!(
            friendly_byte_buf_body(
                "writeIntArray",
                "([I)Lnet/minecraft/network/FriendlyByteBuf;"
            )
            .ends_with("return this;")
        );
        // The 26.3 spelling and the retired one write the same VarInt form.
        assert!(
            friendly_byte_buf_body("writeVarIntArray", "([I)V").contains("horizonPutVarInt(value)")
        );
        assert!(
            friendly_byte_buf_body("writeIntArray", "([I)V").contains("horizonPutVarInt(value)")
        );
        assert!(!friendly_byte_buf_body("writeIntArray", "([I)V").contains("horizonPutInt(value)"));
        assert!(friendly_byte_buf_body("readVarIntArray", "()[I").contains("horizonGetVarInt()"));
        assert!(friendly_byte_buf_body("readDoubleArray", "()[D").contains("horizonGetLong()"));
        assert_eq!(
            friendly_byte_buf_body("writeFloatLE", "(F)V"),
            "horizonPutIntLE(Float.floatToRawIntBits(p0));"
        );
        assert_eq!(
            friendly_byte_buf_body("readDoubleLE", "()D"),
            "return Double.longBitsToDouble(horizonGetLongLE());"
        );
        assert_eq!(
            friendly_byte_buf_body("readerIndex", "(I)I"),
            "return horizonSetReaderIndexView(p0);"
        );
        assert_eq!(
            friendly_byte_buf_body("setIndex", "(I)V"),
            "horizonSetIndexView(p0);"
        );
        assert!(
            friendly_byte_buf_body(
                "readResourceLocation",
                "()Lnet/minecraft/resources/Identifier;"
            )
            .contains("new net.minecraft.resources.Identifier(value.substring(0, separator)")
        );
        assert!(
            friendly_byte_buf_body(
                "writeBlockPos",
                "(Lnet/minecraft/core/BlockPos;)Lnet/minecraft/network/FriendlyByteBuf;"
            )
            .contains("horizonPutLong(((long) p0.getX() & 0x3FFFFFFL) << 38")
        );
        assert_eq!(
            friendly_byte_buf_body(
                "writeChunkPos",
                "(Lnet/minecraft/core/ChunkPos;)Lnet/minecraft/network/FriendlyByteBuf;"
            ),
            "horizonPutInt(p0.x); horizonPutInt(p0.z); return this;"
        );
        assert_eq!(
            friendly_byte_buf_body("readChunkPos", "()Lnet/minecraft/core/ChunkPos;"),
            "return new net.minecraft.core.ChunkPos(horizonGetInt(), horizonGetInt());"
        );
        assert_eq!(
            friendly_byte_buf_body(
                "writeByteArray",
                "([B)Lnet/minecraft/network/FriendlyByteBuf;"
            ),
            "horizonPutVarInt(p0 == null ? 0 : p0.length); horizonPutBytes(p0); return this;"
        );
        assert_eq!(
            friendly_byte_buf_body(
                "readNullable",
                "(Ljava/util/function/Function;)Ljava/lang/Object;"
            ),
            "return horizonGet() == 0 ? null : p0.apply(this);"
        );
        // The enum pair keeps working, now as the ordinal VarInt vanilla writes.
        assert_eq!(
            friendly_byte_buf_body(
                "writeEnum",
                "(Ljava/lang/Enum;)Lnet/minecraft/network/FriendlyByteBuf;"
            ),
            "horizonPutVarInt(p0 == null ? -1 : p0.ordinal()); return this;"
        );
        let read_enum = friendly_byte_buf_body("readEnum", "(Ljava/lang/Class;)Ljava/lang/Enum;");
        assert!(read_enum.contains("p0.getEnumConstants()"), "{read_enum}");
        assert!(
            read_enum.contains("(java.lang.Enum) constants[ordinal]"),
            "{read_enum}"
        );
        // Opaque values keep the kind-checked marker FIFO.
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/network/FriendlyByteBuf",
                "writeNbt",
                "net.minecraft.network.FriendlyByteBuf",
                "(Lnet/minecraft/nbt/Tag;)Lnet/minecraft/network/FriendlyByteBuf;",
            ),
            "horizonValues.add(p0); return this;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/network/FriendlyByteBuf",
                "readNbt",
                "net.minecraft.nbt.Tag",
                "()Lnet/minecraft/nbt/Tag;",
            ),
            "return horizonReadIndex < horizonValues.size() ? (net.minecraft.nbt.Tag) horizonValues.get(horizonReadIndex++) : null;"
        );
    }

    /// The probe `main` the compile test runs: every bounded pair must survive
    /// a write → read round trip at the same cursor.
    const FRIENDLY_BYTE_BUF_PROBE: &str = r#"    public static void main(String[] args) {
        FriendlyByteBuf buf = new FriendlyByteBuf();
        buf.writeByte(-7).writeShort(-300).writeInt(-123456789).writeVarInt(1234567);
        buf.writeLong(-9007199254740993L).writeVarLong(1234567890123L);
        buf.writeFloat(1.5f).writeDouble(-2.25d).writeBoolean(true).writeChar('Z');
        buf.writeUtf("h\u00e9llo").writeUtf("bounded", 16);
        buf.writeString("native string");
        buf.writeResourceLocation(new net.minecraft.resources.Identifier("mod", "payload"));
        buf.writeUUID(new java.util.UUID(1L, 2L));
        buf.writeBlockPos(new net.minecraft.core.BlockPos(-1000, 64, 2000));
        buf.writeChunkPos(new net.minecraft.core.ChunkPos(-5, 6));
        buf.writeByteArray(new byte[] {1, 2, 3});
        buf.writeString("string alias");
        buf.writeBooleanArray(new boolean[] {true, false, true});
        buf.writeIntArray(new int[] {-7, 42});
        buf.writeVarIntArray(new int[] {-7, 42});
        buf.writeLongArray(new long[] {9L, -11L});
        buf.writeFloatArray(new float[] {1.25f, -2.5f});
        buf.writeDoubleArray(new double[] {3.5d, -4.75d});
        buf.writeStringArray(new String[] {"one", "two"});
        buf.writeBooleanArray(new boolean[] {true, false, true});
        buf.writeIntArray(new int[] {-1, 2, 3});
        buf.writeVarIntArray(new int[] {-1, 2, 3});
        buf.writeLongArray(new long[] {4L, -5L});
        buf.writeFloatArray(new float[] {6.5f, -7.25f});
        buf.writeDoubleArray(new double[] {8.5d, -9.25d});
        buf.writeStringArray(new String[] {"a", "b"});
        if (buf.readByte() != -7) throw new AssertionError("byte");
        if (buf.readShort() != -300) throw new AssertionError("short");
        if (buf.readInt() != -123456789) throw new AssertionError("int");
        if (buf.readVarInt() != 1234567) throw new AssertionError("varint");
        if (buf.readLong() != -9007199254740993L) throw new AssertionError("long");
        if (buf.readVarLong() != 1234567890123L) throw new AssertionError("varlong");
        if (buf.readFloat() != 1.5f) throw new AssertionError("float");
        if (buf.readDouble() != -2.25d) throw new AssertionError("double");
        if (!buf.readBoolean()) throw new AssertionError("boolean");
        if (buf.readChar() != 'Z') throw new AssertionError("char");
        if (!"h\u00e9llo".equals(buf.readUtf())) throw new AssertionError("utf");
        if (!"bounded".equals(buf.readUtf(16))) throw new AssertionError("utf max");
        if (!"native string".equals(buf.readString())) throw new AssertionError("string");
        if (!"mod:payload".equals(buf.readResourceLocation().toString())) throw new AssertionError("identifier");
        java.util.UUID id = buf.readUUID();
        if (id.getMostSignificantBits() != 1L || id.getLeastSignificantBits() != 2L) throw new AssertionError("uuid");
        net.minecraft.core.BlockPos pos = buf.readBlockPos();
        if (pos.getX() != -1000 || pos.getY() != 64 || pos.getZ() != 2000) throw new AssertionError("blockpos " + pos);
        net.minecraft.core.ChunkPos chunk = buf.readChunkPos();
        if (chunk.x != -5 || chunk.z != 6) throw new AssertionError("chunkpos");
        byte[] raw = buf.readByteArray();
        if (raw.length != 3 || raw[2] != 3) throw new AssertionError("byte array");
        if (!"string alias".equals(buf.readString())) throw new AssertionError("string alias");
        if (!java.util.Arrays.equals(buf.readBooleanArray(), new boolean[] {true, false, true})) throw new AssertionError("boolean array");
        if (!java.util.Arrays.equals(buf.readIntArray(), new int[] {-7, 42})) throw new AssertionError("int array");
        if (!java.util.Arrays.equals(buf.readVarIntArray(), new int[] {-7, 42})) throw new AssertionError("varint array");
        if (!java.util.Arrays.equals(buf.readLongArray(), new long[] {9L, -11L})) throw new AssertionError("long array");
        if (!java.util.Arrays.equals(buf.readFloatArray(), new float[] {1.25f, -2.5f})) throw new AssertionError("float array");
        if (!java.util.Arrays.equals(buf.readDoubleArray(), new double[] {3.5d, -4.75d})) throw new AssertionError("double array");
        if (!java.util.Arrays.equals(buf.readStringArray(), new String[] {"one", "two"})) throw new AssertionError("string array");
        boolean[] booleans = buf.readBooleanArray();
        if (booleans.length != 3 || !booleans[0] || booleans[1] || !booleans[2]) throw new AssertionError("boolean array");
        int[] ints = buf.readIntArray();
        if (ints.length != 3 || ints[0] != -1 || ints[2] != 3) throw new AssertionError("int array");
        int[] varints = buf.readVarIntArray();
        if (varints.length != 3 || varints[0] != -1 || varints[2] != 3) throw new AssertionError("varint array");
        long[] longs = buf.readLongArray();
        if (longs.length != 2 || longs[1] != -5L) throw new AssertionError("long array");
        float[] floats = buf.readFloatArray();
        if (floats.length != 2 || floats[0] != 6.5f || floats[1] != -7.25f) throw new AssertionError("float array");
        double[] doubles = buf.readDoubleArray();
        if (doubles.length != 2 || doubles[0] != 8.5d || doubles[1] != -9.25d) throw new AssertionError("double array");
        String[] strings = buf.readStringArray();
        if (strings.length != 2 || !"a".equals(strings[0]) || !"b".equals(strings[1])) throw new AssertionError("string array");
        if (buf.readableBytes() != 0 || buf.isReadable()) throw new AssertionError("drained " + buf.readableBytes());
        FriendlyByteBuf little = new FriendlyByteBuf();
        little.writeShortLE(-300).writeMedium(-0x12345).writeMediumLE(-0x12345);
        little.writeIntLE(0x01020304).writeLongLE(-9007199254740993L);
        little.writeFloatLE(1.25f);
        little.writeDoubleLE(-4.5d);
        little.writeFloatLE(1.25f);
        little.writeDoubleLE(-3.5d);
        if (little.readShortLE() != -300) throw new AssertionError("short le");
        if (little.readMedium() != -0x12345) throw new AssertionError("medium");
        if (little.readMediumLE() != -0x12345) throw new AssertionError("medium le");
        if (little.readIntLE() != 0x01020304) throw new AssertionError("int le");
        if (little.readLongLE() != -9007199254740993L) throw new AssertionError("long le");
        if (little.readFloatLE() != 1.25f) throw new AssertionError("float le");
        if (little.readDoubleLE() != -4.5d) throw new AssertionError("double le");
        if (little.readFloatLE() != 1.25f) throw new AssertionError("float le");
        if (little.readDoubleLE() != -3.5d) throw new AssertionError("double le");
        java.util.function.BiConsumer<FriendlyByteBuf, String> writeString = (b, value) -> b.writeUtf(value);
        java.util.function.Function<FriendlyByteBuf, Object> readString = b -> b.readUtf();
        FriendlyByteBuf second = new FriendlyByteBuf();
        second.writeEnum(java.time.DayOfWeek.THURSDAY).writeNullable("value", writeString);
        second.writeOptional(java.util.Optional.of("opt"), writeString);
        if (second.readEnum(java.time.DayOfWeek.class) != java.time.DayOfWeek.THURSDAY) throw new AssertionError("enum");
        if (!"value".equals(second.readNullable(readString))) throw new AssertionError("nullable");
        if (!java.util.Optional.of("opt").equals(second.readOptional(readString))) throw new AssertionError("optional");
        if (new FriendlyByteBuf().readNullable(readString) != null) throw new AssertionError("empty nullable");
        if (!new FriendlyByteBuf().readUtf().isEmpty()) throw new AssertionError("empty utf");
        if (new FriendlyByteBuf().readInt() != 0) throw new AssertionError("empty int");
        FriendlyByteBuf cursor = new FriendlyByteBuf();
        cursor.writeInt(0x01020304);
        if (cursor.writerIndex() != 4 || cursor.readerIndex() != 0 || cursor.remaining() != 4) throw new AssertionError("index");
        cursor.writerIndex(2);
        if (cursor.readUnsignedShort() != 0x0102) throw new AssertionError("writer truncate");
        cursor.discardReadBytes();
        if (cursor.readerIndex() != 0 || cursor.readableBytes() != 0) throw new AssertionError("discard");
        cursor.writeInt(42);
        cursor.skipBytes(1);
        if (cursor.readableBytes() != 3) throw new AssertionError("skip");
        cursor.readerIndex(0);
        cursor.discardReadBytes();
        if (cursor.readInt() != 42) throw new AssertionError("compact read");
        cursor.clear();
        if (cursor.writerIndex() != 0 || cursor.readerIndex() != 0) throw new AssertionError("clear");
        FriendlyByteBuf indexed = new FriendlyByteBuf();
        indexed.writeInt(99);
        if (indexed.readerIndex(2) != 0) throw new AssertionError("reader previous");
        if (indexed.readerIndex() != 2) throw new AssertionError("reader view");
        if (indexed.writerIndex(99) != 4) throw new AssertionError("writer previous");
        if (indexed.writerIndex() != 4) throw new AssertionError("writer view");
        indexed.setIndex(1);
        if (indexed.readerIndex() != 1 || indexed.writerIndex() != 1) throw new AssertionError("set index");
        System.out.println("horizon FriendlyByteBuf round-trip ok");
    }
"#;

    /// Minimal game-value stubs so the generated bodies compile on their own.
    const FRIENDLY_BYTE_BUF_STUBS: &[(&str, &str)] = &[
        (
            "net/minecraft/resources/Identifier.java",
            "package net.minecraft.resources;\npublic class Identifier {\n    private String horizonNamespace = \"minecraft\";\n    private String horizonPath = \"\";\n    public Identifier() {}\n    public Identifier(String namespace, String path) { this.horizonNamespace = namespace; this.horizonPath = path; }\n    public String getNamespace() { return horizonNamespace; }\n    public String getPath() { return horizonPath; }\n    public String toString() { return horizonNamespace + \":\" + horizonPath; }\n}\n",
        ),
        (
            "net/minecraft/core/BlockPos.java",
            "package net.minecraft.core;\npublic class BlockPos {\n    private int horizonX;\n    private int horizonY;\n    private int horizonZ;\n    public BlockPos() {}\n    public BlockPos(int x, int y, int z) { this.horizonX = x; this.horizonY = y; this.horizonZ = z; }\n    public int getX() { return horizonX; }\n    public int getY() { return horizonY; }\n    public int getZ() { return horizonZ; }\n    public String toString() { return horizonX + \",\" + horizonY + \",\" + horizonZ; }\n}\n",
        ),
        (
            "net/minecraft/core/ChunkPos.java",
            "package net.minecraft.core;\npublic class ChunkPos {\n    public final int x;\n    public final int z;\n    public ChunkPos(int x, int z) { this.x = x; this.z = z; }\n}\n",
        ),
        (
            "net/minecraft/nbt/Tag.java",
            "package net.minecraft.nbt;\npublic class Tag {}\n",
        ),
    ];

    /// A game jar holding the classes a payload-codec target names, in the
    /// shapes the façade reads (`classes::read_jar`): the codec pair, the
    /// buffer, the two callbacks `StreamCodec.of` takes, the factories of
    /// `ByteBufCodecs`, and the values a mod writes through them.
    ///
    /// A class file's constant pool is where the façade learns a shape, so each
    /// class carries a real method body (`PoolBuilder`) that names its members,
    /// constructors included.
    fn stream_codec_game_jar(path: &Path) {
        use crate::classes::tests_support::PoolBuilder;
        let file = fs::File::create(path).expect("create the game jar");
        let mut jar = zip::ZipWriter::new(file);
        let mut write = |name: &str, bytes: &[u8]| {
            jar.start_file(
                format!("{name}.class"),
                zip::write::SimpleFileOptions::default(),
            )
            .expect("start the game jar entry");
            std::io::Write::write_all(&mut jar, bytes).expect("write the game class");
        };
        // A call over a two-byte pool index, then a `return`. The pool reader
        // takes the names, not the stack depth.
        let call = |index: u16, opcode: u8| {
            let mut code = vec![opcode];
            code.extend(index.to_be_bytes());
            code.push(0xb1);
            code
        };
        let _ = &call;
        // Every type a signature names travels with its member entry, so the
        // class's `refs` hold the types the façade has to resolve.
        let mut codec = PoolBuilder::default();
        // The types the signatures name, so the class's `refs` hold them.
        for named in [
            "net/minecraft/network/codec/StreamEncoder",
            "net/minecraft/network/codec/StreamDecoder",
            "net/minecraft/network/FriendlyByteBuf",
        ] {
            codec.class(named);
        }
        let of = codec.method(
            "net/minecraft/network/codec/StreamCodec",
            "of",
            "(Lnet/minecraft/network/codec/StreamEncoder;Lnet/minecraft/network/codec/StreamDecoder;)Lnet/minecraft/network/codec/StreamCodec;",
        );
        let unit = codec.method(
            "net/minecraft/network/codec/StreamCodec",
            "unit",
            "(Ljava/lang/Object;)Lnet/minecraft/network/codec/StreamCodec;",
        );
        let map = codec.method(
            "net/minecraft/network/codec/StreamCodec",
            "map",
            "(Ljava/util/function/Function;Ljava/util/function/Function;)Lnet/minecraft/network/codec/StreamCodec;",
        );
        let dispatch = codec.method(
            "net/minecraft/network/codec/StreamCodec",
            "dispatch",
            "(Ljava/util/function/Function;Ljava/util/function/Function;)Lnet/minecraft/network/codec/StreamCodec;",
        );
        let recursive = codec.method(
            "net/minecraft/network/codec/StreamCodec",
            "recursive",
            "(Ljava/util/function/UnaryOperator;)Lnet/minecraft/network/codec/StreamCodec;",
        );
        let composite = codec.method(
            "net/minecraft/network/codec/StreamCodec",
            "composite",
            "(Lnet/minecraft/network/codec/StreamCodec;Ljava/util/function/Function;Lnet/minecraft/network/codec/StreamCodec;Ljava/util/function/Function;Ljava/util/function/BiFunction;)Lnet/minecraft/network/codec/StreamCodec;",
        );
        let encode = codec.method(
            "net/minecraft/network/codec/StreamCodec",
            "encode",
            "(Lnet/minecraft/network/FriendlyByteBuf;Ljava/lang/Object;)V",
        );
        let decode = codec.method(
            "net/minecraft/network/codec/StreamCodec",
            "decode",
            "(Lnet/minecraft/network/FriendlyByteBuf;)Ljava/lang/Object;",
        );
        let init = codec.method("net/minecraft/network/codec/StreamCodec", "<init>", "()V");
        let mut body = vec![0x01]; // aconst_null, then a null receiver call
        for index in [encode, decode, init] {
            body.extend(call(index, 0xb6));
        }
        for index in [of, unit, map, dispatch, recursive, composite] {
            body.extend(call(index, 0xb8));
        }
        // The factories as real 26.3 shapes: `unit`/`recursive`/`composite`
        // static, `map`/`dispatch` instance, `of` static.
        let bytes = codec.build_with_members(
            "net/minecraft/network/codec/StreamCodec",
            "java/lang/Object",
            &[
                (
                    "of",
                    "(Lnet/minecraft/network/codec/StreamEncoder;Lnet/minecraft/network/codec/StreamDecoder;)Lnet/minecraft/network/codec/StreamCodec;",
                    0x0009,
                ),
                (
                    "unit",
                    "(Ljava/lang/Object;)Lnet/minecraft/network/codec/StreamCodec;",
                    0x0009,
                ),
                (
                    "map",
                    "(Ljava/util/function/Function;Ljava/util/function/Function;)Lnet/minecraft/network/codec/StreamCodec;",
                    0x0001,
                ),
                (
                    "dispatch",
                    "(Ljava/util/function/Function;Ljava/util/function/Function;)Lnet/minecraft/network/codec/StreamCodec;",
                    0x0001,
                ),
                (
                    "recursive",
                    "(Ljava/util/function/UnaryOperator;)Lnet/minecraft/network/codec/StreamCodec;",
                    0x0009,
                ),
                (
                    "composite",
                    "(Lnet/minecraft/network/codec/StreamCodec;Ljava/util/function/Function;Lnet/minecraft/network/codec/StreamCodec;Ljava/util/function/Function;Ljava/util/function/BiFunction;)Lnet/minecraft/network/codec/StreamCodec;",
                    0x0009,
                ),
                (
                    "encode",
                    "(Lnet/minecraft/network/FriendlyByteBuf;Ljava/lang/Object;)V",
                    0x0001,
                ),
                (
                    "decode",
                    "(Lnet/minecraft/network/FriendlyByteBuf;)Ljava/lang/Object;",
                    0x0001,
                ),
            ],
            &[("horizonShapes", &body)],
        );
        write("net/minecraft/network/codec/StreamCodec", &bytes);

        let mut encoders = PoolBuilder::default();
        encoders.class("net/minecraft/network/FriendlyByteBuf");
        let write_call = encoders.interface_method(
            "net/minecraft/network/codec/StreamEncoder",
            "encode",
            "(Lnet/minecraft/network/FriendlyByteBuf;Ljava/lang/Object;)V",
        );
        let mut body = vec![0x01];
        body.extend(call(write_call, 0xb9));
        let bytes = encoders.build(
            "net/minecraft/network/codec/StreamEncoder",
            "java/lang/Object",
            &[("horizonShapes", &body)],
        );
        write("net/minecraft/network/codec/StreamEncoder", &bytes);

        let mut decoders = PoolBuilder::default();
        decoders.class("net/minecraft/network/FriendlyByteBuf");
        let read_call = decoders.interface_method(
            "net/minecraft/network/codec/StreamDecoder",
            "decode",
            "(Lnet/minecraft/network/FriendlyByteBuf;)Ljava/lang/Object;",
        );
        let mut body = vec![0x01];
        body.extend(call(read_call, 0xb9));
        let bytes = decoders.build(
            "net/minecraft/network/codec/StreamDecoder",
            "java/lang/Object",
            &[("horizonShapes", &body)],
        );
        write("net/minecraft/network/codec/StreamDecoder", &bytes);

        let mut factories = PoolBuilder::default();
        factories.class("net/minecraft/network/codec/StreamCodec");
        factories.class("net/minecraft/network/FriendlyByteBuf");
        let bool_codec = factories.method(
            "net/minecraft/network/codec/ByteBufCodecs",
            "BOOL",
            "()Lnet/minecraft/network/codec/StreamCodec;",
        );
        let int_codec = factories.method(
            "net/minecraft/network/codec/ByteBufCodecs",
            "INT",
            "()Lnet/minecraft/network/codec/StreamCodec;",
        );
        let string_codec = factories.method(
            "net/minecraft/network/codec/ByteBufCodecs",
            "STRING_UTF8",
            "()Lnet/minecraft/network/codec/StreamCodec;",
        );
        let mut body = vec![0x01];
        for index in [bool_codec, int_codec, string_codec] {
            body.extend(call(index, 0xb8));
        }
        // The three factories as real 26.3 shapes: public static methods
        // returning a codec, named by the body above.
        let bytes = factories.build_with_members(
            "net/minecraft/network/codec/ByteBufCodecs",
            "java/lang/Object",
            &[
                (
                    "BOOL",
                    "()Lnet/minecraft/network/codec/StreamCodec;",
                    0x0009,
                ),
                ("INT", "()Lnet/minecraft/network/codec/StreamCodec;", 0x0009),
                (
                    "STRING_UTF8",
                    "()Lnet/minecraft/network/codec/StreamCodec;",
                    0x0009,
                ),
            ],
            &[("horizonShapes", &body)],
        );
        write("net/minecraft/network/codec/ByteBufCodecs", &bytes);

        let mut buffer = PoolBuilder::default();
        for named in [
            "net/minecraft/network/FriendlyByteBuf",
            "net/minecraft/network/codec/StreamCodec",
        ] {
            buffer.class(named);
        }
        let read_boolean = buffer.method(
            "net/minecraft/network/FriendlyByteBuf",
            "readBoolean",
            "()Z",
        );
        let write_boolean = buffer.method(
            "net/minecraft/network/FriendlyByteBuf",
            "writeBoolean",
            "(Z)Lnet/minecraft/network/FriendlyByteBuf;",
        );
        let read_var_int =
            buffer.method("net/minecraft/network/FriendlyByteBuf", "readVarInt", "()I");
        let write_var_int = buffer.method(
            "net/minecraft/network/FriendlyByteBuf",
            "writeVarInt",
            "(I)Lnet/minecraft/network/FriendlyByteBuf;",
        );
        let read_utf = buffer.method(
            "net/minecraft/network/FriendlyByteBuf",
            "readUtf",
            "()Ljava/lang/String;",
        );
        let write_utf = buffer.method(
            "net/minecraft/network/FriendlyByteBuf",
            "writeUtf",
            "(Ljava/lang/String;)Lnet/minecraft/network/FriendlyByteBuf;",
        );
        let reader = buffer.method(
            "net/minecraft/network/FriendlyByteBuf",
            "readerIndex",
            "()I",
        );
        let readable = buffer.method(
            "net/minecraft/network/FriendlyByteBuf",
            "readableBytes",
            "()I",
        );
        let writer = buffer.method(
            "net/minecraft/network/FriendlyByteBuf",
            "writerIndex",
            "()I",
        );
        let buffer_init = buffer.method("net/minecraft/network/FriendlyByteBuf", "<init>", "()V");
        let mut body = vec![0x01];
        for index in [
            read_boolean,
            write_boolean,
            read_var_int,
            write_var_int,
            read_utf,
            write_utf,
            reader,
            readable,
            writer,
        ] {
            body.extend(call(index, 0xb6));
        }
        body.extend(call(buffer_init, 0xb7));
        let bytes = buffer.build(
            "net/minecraft/network/FriendlyByteBuf",
            "java/lang/Object",
            &[("horizonShapes", &body)],
        );
        write("net/minecraft/network/FriendlyByteBuf", &bytes);

        jar.finish().expect("finish the game jar");
    }

    /// The mod's payload class, compiled against the façade: a record sent
    /// through `StreamCodec.composite`, a mapped unit codec, and the
    /// `ByteBufCodecs` factories for its fields.
    /// The mod's payload class, compiled against the façade. The façade's
    /// `StreamCodec` and `ByteBufCodecs` are erased the way the generated
    /// stubs are (the real 26.3 types are generic; their type parameters live
    /// in `Signature` attributes the class reader does not decode), so the
    /// source writes `StreamCodec<Ping>` as the bare type and lets the casts in
    /// the probe do the typing.
    const PAYLOAD_CODEC_TARGET: &str = r#"package demo;

import net.minecraft.network.FriendlyByteBuf;
import net.minecraft.network.codec.ByteBufCodecs;
import net.minecraft.network.codec.StreamCodec;

public final class PingPayload {
    public record Ping(boolean flag, int count) {
        public static final StreamCodec CODEC = StreamCodec.composite(
                ByteBufCodecs.BOOL(), (Object ping) -> ((Ping) ping).flag(),
                ByteBufCodecs.INT(), (Object ping) -> Integer.valueOf(((Ping) ping).count()),
                (Object flag, Object count) -> new Ping((Boolean) flag, (Integer) count));
    }

    /** A composite whose members are themselves composed: a mapped codec
     *  first, then a plain one, so the member order and each member's own
     *  value are both exercised. */
    public record Swap(boolean inverted, int count) {
        public static final StreamCodec CODEC = StreamCodec.composite(
                ByteBufCodecs.BOOL().map(
                        (Object flag) -> Boolean.valueOf(!((Boolean) flag)),
                        (Object flag) -> Boolean.valueOf(!((Boolean) flag))),
                (Object value) -> ((Swap) value).inverted(),
                ByteBufCodecs.INT(),
                (Object value) -> Integer.valueOf(((Swap) value).count()),
                (Object flag, Object count) -> new Swap((Boolean) flag, (Integer) count));
    }

    public static final StreamCodec FLAG = ByteBufCodecs.BOOL();

    public static final StreamCodec NAME = ByteBufCodecs.STRING_UTF8();
}
"#;

    /// The mod's driver, run after the façade: encode and decode through every
    /// codec shape the slice builds, at the same buffer cursor.
    const PAYLOAD_CODEC_PROBE: &str = r#"package demo;

import net.minecraft.network.FriendlyByteBuf;
import net.minecraft.network.codec.ByteBufCodecs;
import net.minecraft.network.codec.StreamCodec;

public final class CodecProbe {
    @SuppressWarnings("unchecked")
    public static void main(String[] args) {
        FriendlyByteBuf buffer = new FriendlyByteBuf();
        PingPayload.Ping sent = new PingPayload.Ping(true, 1234567);
        PingPayload.Ping.CODEC.encode(buffer, sent);
        PingPayload.Ping got = (PingPayload.Ping) PingPayload.Ping.CODEC.decode(buffer);
        if (!got.flag() || got.count() != 1234567) throw new AssertionError("composite " + got);
        if (buffer.readableBytes() != 0) throw new AssertionError("composite left " + buffer.readableBytes() + " bytes");
        FriendlyByteBuf single = new FriendlyByteBuf();
        PingPayload.FLAG.encode(single, true);
        PingPayload.FLAG.encode(single, false);
        if (!((Boolean) PingPayload.FLAG.decode(single)) || ((Boolean) PingPayload.FLAG.decode(single))) throw new AssertionError("bool");
        // A composite over a mapped member: `inverted` is written through the
        // map's `from` and read back through its `to`, so `true` survives.
        PingPayload.Swap swapSent = new PingPayload.Swap(true, 42);
        FriendlyByteBuf swapBuffer = new FriendlyByteBuf();
        PingPayload.Swap.CODEC.encode(swapBuffer, swapSent);
        PingPayload.Swap swapGot = (PingPayload.Swap) PingPayload.Swap.CODEC.decode(swapBuffer);
        if (!swapGot.inverted() || swapGot.count() != 42) throw new AssertionError("nested composite " + swapGot);
        if (swapBuffer.readableBytes() != 0) throw new AssertionError("nested composite left bytes");
        FriendlyByteBuf named = new FriendlyByteBuf();
        PingPayload.NAME.encode(named, "h\u00e9llo");
        if (!"h\u00e9llo".equals((String) PingPayload.NAME.decode(named))) throw new AssertionError("string");
        StreamCodec count = ByteBufCodecs.INT();
        FriendlyByteBuf number = new FriendlyByteBuf();
        count.encode(number, Integer.valueOf(-7));
        if (((Integer) count.decode(number)).intValue() != -7) throw new AssertionError("int");
        StreamCodec mapped = ByteBufCodecs.BOOL().map(
                (Object flag) -> Boolean.valueOf(!((Boolean) flag)),
                (Object flag) -> Boolean.valueOf(!((Boolean) flag)));
        FriendlyByteBuf mappedBuffer = new FriendlyByteBuf();
        mapped.encode(mappedBuffer, Boolean.TRUE);
        // `map(to, from)`: `from` flips on the way out, `to` flips on the way
        // back, so a `true` written reads back `true`.
        if (!((Boolean) mapped.decode(mappedBuffer)).booleanValue()) throw new AssertionError("map");
        StreamCodec unit = StreamCodec.unit("fixed");
        FriendlyByteBuf empty = new FriendlyByteBuf();
        unit.encode(empty, "ignored");
        if (empty.writerIndex() != 0 || !"fixed".equals(unit.decode(empty))) throw new AssertionError("unit");
        System.out.println("horizon payload codecs ok");
    }
}
"#;

    /// The whole slice, through the generator and the JVM: the façade is
    /// generated from a game jar with the real 26.3 shapes, compiled, and the
    /// mod's payload load runs against it. A codec that only linked would fail
    /// here on the first `encode`, which is what the old placeholder object
    /// did.
    #[test]
    fn payload_codecs_compile_and_round_trip_through_the_facade() {
        use std::{path::PathBuf, process::Command};
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("horizon-port-pack-codecs-{stamp}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create the scratch root");
        let game_jar = root.join("game-26.3.jar");
        stream_codec_game_jar(&game_jar);
        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let javac = jdk_bin
            .as_ref()
            .map(|bin| bin.join("javac"))
            .unwrap_or_else(|| PathBuf::from("javac"));
        let java = jdk_bin
            .as_ref()
            .map(|bin| bin.join("java"))
            .unwrap_or_else(|| PathBuf::from("java"));
        // The target jar: one class that names the codec surface.
        let target = root.join("demo-mod.jar");
        {
            use crate::classes::tests_support::PoolBuilder;
            let file = fs::File::create(&target).expect("create the target jar");
            let mut jar = zip::ZipWriter::new(file);
            jar.start_file(
                "demo/PingPayload.class",
                zip::write::SimpleFileOptions::default(),
            )
            .expect("start the target entry");
            let mut pool = PoolBuilder::default();
            let composite = pool.method(
                "net/minecraft/network/codec/StreamCodec",
                "composite",
                "(Lnet/minecraft/network/codec/StreamCodec;Ljava/util/function/Function;Lnet/minecraft/network/codec/StreamCodec;Ljava/util/function/Function;Ljava/util/function/BiFunction;)Lnet/minecraft/network/codec/StreamCodec;",
            );
            let unit = pool.method(
                "net/minecraft/network/codec/StreamCodec",
                "unit",
                "(Ljava/lang/Object;)Lnet/minecraft/network/codec/StreamCodec;",
            );
            let map = pool.method(
                "net/minecraft/network/codec/StreamCodec",
                "map",
                "(Ljava/util/function/Function;Ljava/util/function/Function;)Lnet/minecraft/network/codec/StreamCodec;",
            );
            let boolean_codec = pool.method(
                "net/minecraft/network/codec/ByteBufCodecs",
                "BOOL",
                "()Lnet/minecraft/network/codec/StreamCodec;",
            );
            let int_codec = pool.method(
                "net/minecraft/network/codec/ByteBufCodecs",
                "INT",
                "()Lnet/minecraft/network/codec/StreamCodec;",
            );
            let string_codec = pool.method(
                "net/minecraft/network/codec/ByteBufCodecs",
                "STRING_UTF8",
                "()Lnet/minecraft/network/codec/StreamCodec;",
            );
            let mut body = vec![0x01];
            for (opcode, index) in [
                (0xb8, composite),
                (0xb8, unit),
                (0xb6, map),
                (0xb8, boolean_codec),
                (0xb8, int_codec),
                (0xb8, string_codec),
            ] {
                body.push(opcode);
                body.extend(index.to_be_bytes());
            }
            // The buffer calls the payload's codec makes on the mod's behalf.
            for (name, descriptor) in [
                ("readBoolean", "()Z"),
                ("writeBoolean", "(Z)Lnet/minecraft/network/FriendlyByteBuf;"),
                ("readVarInt", "()I"),
                ("writeVarInt", "(I)Lnet/minecraft/network/FriendlyByteBuf;"),
                ("readUtf", "()Ljava/lang/String;"),
                (
                    "writeUtf",
                    "(Ljava/lang/String;)Lnet/minecraft/network/FriendlyByteBuf;",
                ),
                ("readableBytes", "()I"),
                ("writerIndex", "()I"),
                ("<init>", "()V"),
            ] {
                let index = pool.method("net/minecraft/network/FriendlyByteBuf", name, descriptor);
                body.push(0xb6);
                body.extend(index.to_be_bytes());
            }
            body.push(0xb1);
            std::io::Write::write_all(
                &mut jar,
                &pool.build(
                    "demo/PingPayload",
                    "java/lang/Object",
                    &[("horizonShapes", &body)],
                ),
            )
            .expect("write the target class");
            jar.finish().expect("finish the target jar");
        }
        // The other façade tests skip when there is no JDK, because their
        // source assertions still hold. This one is the only check that a
        // codec actually round-trips, so a missing toolchain must fail loudly
        // rather than pass with nothing run.
        assert!(
            Command::new(&javac).arg("-version").output().is_ok(),
            "no javac on this machine: the codec slice is unverified"
        );
        let out = root.join("facade");
        let report = generate(&target, &game_jar, &[], &out, &javac, &PathBuf::from("jar"))
            .unwrap_or_else(|e| {
                panic!(
                    "the façade did not generate: {e} (sources kept in {})",
                    out.display()
                )
            });
        assert!(
            report.jar.is_file(),
            "the façade jar was not written: {report:?}"
        );
        // The mod's own classes, compiled against the generated façade.
        let sources = root.join("mod-src");
        let classes = root.join("mod-classes");
        fs::create_dir_all(sources.join("demo")).expect("create the mod sources");
        fs::write(sources.join("demo/PingPayload.java"), PAYLOAD_CODEC_TARGET)
            .expect("write the payload");
        fs::write(sources.join("demo/CodecProbe.java"), PAYLOAD_CODEC_PROBE)
            .expect("write the probe");
        fs::create_dir_all(&classes).expect("create the mod classes");
        // The façade's `FriendlyByteBuf` names the Netty buffer in one
        // constructor; a payload never loads that method, and `javac` only
        // needs the type to exist.
        let status = Command::new(&javac)
            .arg("-nowarn")
            .arg("-d")
            .arg(&classes)
            .arg("-classpath")
            .arg(&report.jar)
            .arg(sources.join("demo/PingPayload.java"))
            .arg(sources.join("demo/CodecProbe.java"))
            .status()
            .expect("run javac");
        assert!(
            status.success(),
            "javac rejected the payload compiled against the generated façade"
        );
        let output = Command::new(&java)
            .arg("-cp")
            .arg(format!("{}:{}", report.jar.display(), classes.display()))
            .arg("demo.CodecProbe")
            .output()
            .expect("run the codec probe");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "codec probe failed: {stdout}{stderr}"
        );
        assert!(
            stdout.contains("horizon payload codecs ok"),
            "probe output: {stdout}{stderr}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    fn friendly_byte_buf_java_source() -> String {
        use std::fmt::Write as _;
        // The probe class is the generated shape itself: package, return
        // type, and helper block as the façade writes them.
        let mut source =
            String::from("package net.minecraft.network;\n\npublic class FriendlyByteBuf {\n");
        source.push_str(FRIENDLY_BYTE_BUF_STATE);
        for (method, descriptor) in FRIENDLY_BYTE_BUF_PAIRS {
            // JVM descriptors may distinguish methods by return type, while
            // Java source cannot overload `readerIndex(int)`/`writerIndex(int)`
            // on that basis. Keep both ABI spellings in the body table, but
            // compile the requested int-returning view in the source probe.
            // The fluent and void write spellings are the same erasure for the
            // same reason, so the probe compiles the void one.
            if matches!(
                (*method, *descriptor),
                ("readerIndex", "(I)Lnet/minecraft/network/FriendlyByteBuf;")
                    | ("writerIndex", "(I)Lnet/minecraft/network/FriendlyByteBuf;")
            ) || (descriptor.ends_with(")Lnet/minecraft/network/FriendlyByteBuf;")
                && matches!(
                    *method,
                    "writeString"
                        | "writeBooleanArray"
                        | "writeVarIntArray"
                        | "writeIntArray"
                        | "writeLongArray"
                        | "writeFloatArray"
                        | "writeDoubleArray"
                        | "writeStringArray"
                        | "writeFloatLE"
                        | "writeDoubleLE"
                ))
            {
                continue;
            }
            let (ret, params) = signature(descriptor);
            let body = runtime_body_with_descriptor(
                "net/minecraft/network/FriendlyByteBuf",
                method,
                &ret,
                descriptor,
            );
            let _ = writeln!(source, "    public {ret} {method}({params}) {{ {body} }}");
        }
        source.push_str(FRIENDLY_BYTE_BUF_PROBE);
        source.push_str("}\n");
        source
    }

    #[test]
    fn friendly_byte_buf_bodies_compile_and_round_trip() {
        use std::{path::PathBuf, process::Command};
        let source = friendly_byte_buf_java_source();
        assert!(source.contains("protected byte[] horizonBytes = new byte[0];"));
        assert!(!source.contains("UnsupportedOperationException"));
        let root = std::env::temp_dir().join(format!("horizon-facade-fbb-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        let mut files = Vec::new();
        let mut write = |relative: &str, text: &str| {
            let file = src.join(relative);
            fs::create_dir_all(file.parent().expect("package directory")).expect("create package");
            fs::write(&file, text).expect("write java stub");
            files.push(file);
        };
        for (relative, text) in FRIENDLY_BYTE_BUF_STUBS {
            write(relative, text);
        }
        write("net/minecraft/network/FriendlyByteBuf.java", &source);
        fs::create_dir_all(&classes).expect("create classes directory");
        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let javac = jdk_bin
            .as_ref()
            .map(|bin| bin.join("javac"))
            .unwrap_or_else(|| PathBuf::from("javac"));
        let javac_status = Command::new(&javac)
            .arg("-nowarn")
            .arg("-d")
            .arg(&classes)
            .args(&files)
            .status();
        let Ok(javac_status) = javac_status else {
            // No JDK here: the source assertions above remain the check.
            let _ = fs::remove_dir_all(&root);
            return;
        };
        assert!(
            javac_status.success(),
            "javac rejected the generated FriendlyByteBuf bodies"
        );
        let java = jdk_bin
            .as_ref()
            .map(|bin| bin.join("java"))
            .unwrap_or_else(|| PathBuf::from("java"));
        let output = Command::new(&java)
            .arg("-cp")
            .arg(&classes)
            .arg("net.minecraft.network.FriendlyByteBuf")
            .output()
            .expect("run the generated buffer");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "probe failed: {stderr}");
        assert!(
            stdout.contains("horizon FriendlyByteBuf round-trip ok"),
            "probe output: {stdout}{stderr}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// `ByteBufCodecs`' catalogue as codecs: each primitive pair writes and
    /// reads the same call, a list writes its size before its elements, an
    /// optional writes a presence byte, and a factory this slice does not
    /// model answers `None` so the caller keeps the shape-only codec.
    #[test]
    fn byte_buf_codec_factories_cover_the_bounded_buffer() {
        let bool_codec =
            byte_buf_codec_factory("BOOL", "()Lnet/minecraft/network/codec/StreamCodec;")
                .expect("BOOL is a primitive pair");
        assert!(bool_codec.contains("buffer.writeBoolean(value)"));
        assert!(bool_codec.contains("(java.lang.Boolean) buffer.readBoolean()"));
        // An integer travels as a VarInt, which is what vanilla's `INT` is.
        let int_codec =
            byte_buf_codec_factory("INT", "()Lnet/minecraft/network/codec/StreamCodec;")
                .expect("INT is a primitive pair");
        assert!(int_codec.contains("buffer.writeVarInt(value)"));
        assert!(int_codec.contains("buffer.readVarInt()"));
        let string_codec =
            byte_buf_codec_factory("STRING_UTF8", "()Lnet/minecraft/network/codec/StreamCodec;")
                .expect("STRING_UTF8 is a primitive pair");
        assert!(string_codec.contains("buffer.writeUtf(value)"));
        assert!(string_codec.contains("buffer.readUtf()"));
        // The callbacks are typed: the façade's `ByteBufCodecs` is raw, so a
        // lambda without them would infer `Object` and the buffer call would
        // not resolve.
        for codec in [&bool_codec, &int_codec, &string_codec] {
            assert!(
                codec.contains("net.minecraft.network.FriendlyByteBuf buffer"),
                "{codec}"
            );
        }
        let list = byte_buf_codec_factory(
            "list",
            "(Lnet/minecraft/network/codec/StreamCodec;)Lnet/minecraft/network/codec/StreamCodec;",
        )
        .expect("a list is modelled");
        assert!(list.contains("buffer.writeVarInt(elements.size())"));
        assert!(list.contains("int size = buffer.readVarInt()"));
        let optional = byte_buf_codec_factory(
            "optional",
            "(Lnet/minecraft/network/codec/StreamCodec;)Lnet/minecraft/network/codec/StreamCodec;",
        )
        .expect("an optional is modelled");
        assert!(optional.contains("buffer.writeBoolean(optional.isPresent())"));
        assert!(optional.contains("java.util.Optional.ofNullable(p0.decode(buffer))"));
        // A factory outside the bounded value set keeps the shape-only codec.
        assert!(
            byte_buf_codec_factory("QUATERNION", "()Lnet/minecraft/network/codec/StreamCodec;")
                .is_none()
        );
        assert!(
            byte_buf_codec_factory(
                "registry",
                "(Lnet/minecraft/core/Registry;)Lnet/minecraft/network/codec/StreamCodec;"
            )
            .is_none()
        );
    }

    #[test]
    fn stream_codec_factories_are_bootstrap_values() {
        assert_eq!(
            stream_codec_body(
                "unit",
                "(Ljava/lang/Object;)Lnet/minecraft/network/codec/StreamCodec;"
            )
            .as_deref(),
            Some(
                "net.minecraft.network.codec.StreamCodec codec = new net.minecraft.network.codec.StreamCodec(); codec.horizonValue = p0; return codec;"
            )
        );
        assert_eq!(
            stream_codec_body(
                "of",
                "(Lnet/minecraft/network/codec/StreamEncoder;Lnet/minecraft/network/codec/StreamDecoder;)Lnet/minecraft/network/codec/StreamCodec;"
            )
            .as_deref(),
            Some("net.minecraft.network.codec.StreamCodec codec = new net.minecraft.network.codec.StreamCodec(); return codec.horizonOf(p0, p1);")
        );
        // A composite keeps both members with their getters, in wire order.
        let composite = stream_composite_body(
            "(Lnet/minecraft/network/codec/StreamCodec;Ljava/util/function/Function;Lnet/minecraft/network/codec/StreamCodec;Ljava/util/function/Function;Ljava/util/function/BiFunction;)Lnet/minecraft/network/codec/StreamCodec;",
        );
        assert!(composite.contains("codec.horizonKind = HORIZON_COMPOSITE;"));
        assert!(composite.contains("codec.horizonMember1 = p0;"));
        assert!(composite.contains("codec.horizonGetters1 = p1;"));
        assert!(composite.contains("codec.horizonMember2 = p2;"));
        assert!(composite.contains("codec.horizonGetters2 = p3;"));
        assert!(composite.contains("codec.horizonConstructor = p4;"));
        // Encoders and decoders write and read `horizonMembers()` in order.
        let encode = ENCODE_CODEC;
        let decode = DECODE_CODEC;
        assert!(encode.contains("for (int i = 1; i <= 8; i++)"));
        assert!(encode.contains("horizonGetterAt(i).apply(p1)"));
        assert!(decode.contains("horizonDecodeMembers(p0)"));
        assert_eq!(
            runtime_field_default("", "Lnet/minecraft/network/codec/StreamCodec;", "null"),
            "new net.minecraft.network.codec.StreamCodec()"
        );
        assert_eq!(
            runtime_field_default("", "Lcom/mojang/serialization/Codec;", "null"),
            "com.mojang.serialization.Codec.STRING"
        );
        assert!(
            runtime_field_default(
                "net/minecraft/world/entity/SpawnPlacementTypes",
                "Lnet/minecraft/world/entity/SpawnPlacementType;",
                "null",
            )
            .contains("isSpawnPositionOk")
        );
        assert_eq!(
            runtime_field_default(
                "net/minecraft/core/registries/BuiltInRegistries",
                "Lnet/minecraft/core/Registry;",
                "null",
            ),
            "new net.minecraft.core.Registry() {}"
        );
        assert_eq!(
            runtime_field_default(
                "net/minecraft/core/component/DataComponents",
                "Lnet/minecraft/core/component/DataComponentType;",
                "null",
            ),
            "new net.minecraft.core.component.DataComponentType() {}"
        );
        assert_eq!(
            runtime_body("net/minecraft/core/Registry", "containsKey", "boolean"),
            "return horizonEntries.containsKey(p0);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/Registry",
                "getOptional",
                "java.util.Optional",
                "(Lnet/minecraft/resources/Identifier;)Ljava/util/Optional;",
            ),
            "return java.util.Optional.ofNullable(horizonEntries.get(p0));"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/DefaultedRegistry",
                "getKey",
                "net.minecraft.resources.Identifier",
                "(Ljava/lang/Object;)Lnet/minecraft/resources/Identifier;",
            ),
            "for (java.util.Map.Entry<net.minecraft.resources.Identifier,Object> e : horizonEntries.entrySet()) if (java.util.Objects.equals(e.getValue(), p0)) return e.getKey(); return null;"
        );
        assert!(
            runtime_body("net/minecraft/world/item/Item", "of", "void")
                .contains("UnsupportedOperationException")
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/resources/Identifier",
                "fromNamespaceAndPath",
                "net.minecraft.resources.Identifier"
            ),
            "return new Identifier(p0, p1);"
        );
        assert!(
            runtime_body(
                "net/minecraft/resources/Identifier",
                "parse",
                "net.minecraft.resources.Identifier"
            )
            .contains("indexOf(':')")
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/resources/Identifier",
                "withPrefix",
                "net.minecraft.resources.Identifier",
                "(Ljava/lang/String;)Lnet/minecraft/resources/Identifier;",
            ),
            "return new Identifier(horizonNamespace, p0 + horizonPath);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/resources/Identifier",
                "toLanguageKey",
                "java.lang.String",
                "(Ljava/lang/String;)Ljava/lang/String;",
            ),
            "return p0 + \".\" + horizonNamespace + \".\" + horizonPath.replace('/', '.');"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/resources/Identifier",
                "compareTo",
                "int",
                "(Ljava/lang/Object;)I",
            ),
            "return toString().compareTo(p0.toString());"
        );
        assert!(
            runtime_body("net/minecraft/network/VarInt", "getByteSize", "int")
                .contains("-268435456")
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/network/VarInt",
                "hasContinuationBit",
                "boolean"
            ),
            "return (p0 & 0x80) != 0;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/InteractionResult",
                "consumesAction",
                "boolean"
            ),
            "return this instanceof InteractionResult.Success;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/InteractionResult$Success",
                "shouldSwing",
                "boolean"
            ),
            "return swingSource() != InteractionResult.SwingSource.NONE;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/InteractionResult$Success",
                "heldItemTransformedTo",
                "net.minecraft.world.item.ItemStack",
                "()Lnet/minecraft/world/item/ItemStack;",
            ),
            "return itemContext().heldItemTransformedTo();"
        );
        assert_eq!(
            runtime_field_default_named(
                "net/minecraft/world/InteractionResult",
                "SUCCESS_SERVER",
                "Lnet/minecraft/world/InteractionResult$Success;",
                "null",
            ),
            "new net.minecraft.world.InteractionResult.Success(net.minecraft.world.InteractionResult.SwingSource.SERVER_ONLY, net.minecraft.world.InteractionResult.ItemContext.DEFAULT)"
        );
        assert_eq!(
            runtime_field_default_named(
                "net/minecraft/world/InteractionResult$ItemContext",
                "NONE",
                "Lnet/minecraft/world/InteractionResult$ItemContext;",
                "null",
            ),
            "new net.minecraft.world.InteractionResult.ItemContext(false, null)"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/flag/FeatureFlagSet",
                "of",
                "net.minecraft.world.flag.FeatureFlagSet"
            ),
            "return new FeatureFlagSet();"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/item/Item",
                "getDefaultInstance",
                "net.minecraft.world.item.ItemStack",
                "()Lnet/minecraft/world/item/ItemStack;",
            ),
            "return new net.minecraft.world.item.ItemStack(this, 1);"
        );
        assert_eq!(
            runtime_body("net/minecraft/world/item/ItemStack", "getCount", "int"),
            "return horizonCount;"
        );
        assert_eq!(
            runtime_body("net/minecraft/world/item/ItemStack", "isEmpty", "boolean"),
            "return horizonCount <= 0;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/item/ItemStack",
                "getTags",
                "java.util.stream.Stream",
                "()Ljava/util/stream/Stream;",
            ),
            "return java.util.stream.Stream.empty();"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/item/ItemStack",
                "getMaxStackSize",
                "int"
            ),
            "return horizonItem == null ? 64 : horizonItem.getMaxStackSize();"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/item/ItemStack",
                "isDamageable",
                "boolean"
            ),
            "return horizonItem != null && horizonItem.isDamageable();"
        );
        assert_eq!(
            runtime_body("net/minecraft/world/item/ItemStack", "hasFoil", "boolean"),
            "return false;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/item/ItemStack",
                "getDamageValue",
                "int"
            ),
            "return horizonDamage;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/item/ItemStack",
                "setDamageValue",
                "void"
            ),
            "horizonDamage = Math.max(0, p0);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/item/ItemStack",
                "is",
                "boolean",
                "(Lnet/minecraft/tags/TagKey;)Z",
            ),
            "return false;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/item/ItemStack",
                "isSameItem",
                "boolean",
                "(Lnet/minecraft/world/item/ItemStack;)Z",
            ),
            "return p0 != null && horizonItem == p0.horizonItem;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/item/ItemStack",
                "isSameItemSameTags",
                "boolean",
                "(Lnet/minecraft/world/item/ItemStack;)Z",
            ),
            "return p0 != null && horizonItem == p0.horizonItem && horizonComponents.equals(p0.horizonComponents);"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/item/ItemStack",
                "isEnchanted",
                "boolean"
            ),
            "return false;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/item/ItemStack",
                "getUseAnimation",
                "net.minecraft.world.item.UseAnim"
            ),
            "return net.minecraft.world.item.UseAnim.NONE;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/item/ItemStack",
                "getHoverName",
                "net.minecraft.network.chat.Component",
            ),
            "return net.minecraft.network.chat.Component.literal(horizonItem.toString());"
        );
        assert!(
            runtime_body(
                "net/minecraft/world/item/ItemStack",
                "copy",
                "net.minecraft.world.item.ItemStack"
            )
            .contains("horizonCount")
        );
        assert!(
            runtime_body(
                "net/minecraft/world/item/ItemStack",
                "copy",
                "net.minecraft.world.item.ItemStack"
            )
            .contains("horizonDamage = horizonDamage")
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/util/StringRepresentable",
                "fromEnum",
                "net.minecraft.util.StringRepresentable.EnumCodec"
            ),
            "return new EnumCodec();"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/core/component/DataComponentType",
                "builder",
                "net.minecraft.core.component.DataComponentType.Builder"
            ),
            "return new DataComponentType.Builder();"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/core/component/DataComponentType$Builder",
                "cacheEncoding",
                "net.minecraft.core.component.DataComponentType.Builder"
            ),
            "return this;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/core/Registry",
                "register",
                "java.lang.Object"
            ),
            "return p2;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/entity/ai/attributes/RangedAttribute",
                "setSyncable",
                "net.minecraft.world.entity.ai.attributes.Attribute"
            ),
            "return this;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/core/Registry",
                "registerForHolder",
                "net.minecraft.core.Holder.Reference"
            ),
            "horizonEntries.put(p1, p2); horizonKeys.put(p1, p0); net.minecraft.core.Holder.Reference holder = new net.minecraft.core.Holder.Reference(); holder.horizonBind(p2, p0, p1); return holder;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/effect/MobEffect",
                "addAttributeModifier",
                "net.minecraft.world.effect.MobEffect"
            ),
            "return this;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/level/block/state/BlockBehaviour$Properties",
                "of",
                "net.minecraft.world.level.block.state.BlockBehaviour.Properties"
            ),
            "return new Properties();"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/level/block/state/StateDefinition",
                "any",
                "net.minecraft.world.level.block.state.StateHolder"
            ),
            "return new net.minecraft.world.level.block.state.BlockState();"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/resources/ResourceKey",
                "create",
                "net.minecraft.resources.ResourceKey"
            ),
            "return new ResourceKey(p0 == null ? new net.minecraft.resources.Identifier() : p0.identifier(), p1);"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/resources/ResourceKey",
                "identifier",
                "net.minecraft.resources.Identifier"
            ),
            "return horizonIdentifier;"
        );
        assert_eq!(
            runtime_body("net/minecraft/resources/ResourceKey", "hashCode", "int"),
            "return java.util.Objects.hash(horizonRegistryName, horizonIdentifier);"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/core/DefaultedRegistry",
                "getKey",
                "net.minecraft.resources.Identifier"
            ),
            "for (java.util.Map.Entry<net.minecraft.resources.Identifier,Object> e : horizonEntries.entrySet()) if (java.util.Objects.equals(e.getValue(), p0)) return e.getKey(); return null;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/core/DefaultedRegistry",
                "getResourceKey",
                "java.util.Optional"
            ),
            "net.minecraft.resources.Identifier id = getKey(p0); return id == null ? java.util.Optional.empty() : java.util.Optional.ofNullable(horizonKeys.get(id));"
        );
        assert_eq!(
            runtime_field_default(
                "net/minecraft/resources/RegistryDataLoader",
                "Ljava/util/List;",
                "null"
            ),
            "new java.util.ArrayList()"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/resources/RegistryValidator",
                "none",
                "net.minecraft.resources.RegistryValidator"
            ),
            "return new net.minecraft.resources.RegistryValidator() {};"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/tags/TagKey",
                "create",
                "net.minecraft.tags.TagKey"
            ),
            "return new net.minecraft.tags.TagKey(p0, p1);"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/tags/TagKey",
                "location",
                "net.minecraft.resources.Identifier"
            ),
            "return location;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/tags/TagKey",
                "registry",
                "net.minecraft.resources.ResourceKey"
            ),
            "return registry;"
        );
        assert_eq!(
            runtime_body("net/minecraft/tags/TagKey", "isFor", "boolean"),
            "return java.util.Objects.equals(registry, p0);"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/entity/EntityType",
                "getCategory",
                "net.minecraft.world.entity.MobCategory"
            ),
            "return horizonCategory;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/entity/MobCategory",
                "getName",
                "java.lang.String"
            ),
            "if (name().equals(\"MONSTER\")) return \"monster\"; if (name().equals(\"CREATURE\")) return \"creature\"; if (name().equals(\"AMBIENT\")) return \"ambient\"; if (name().equals(\"AXOLOTLS\")) return \"axolotls\"; if (name().equals(\"UNDERGROUND_WATER_CREATURE\")) return \"underground_water_creature\"; if (name().equals(\"WATER_CREATURE\")) return \"water_creature\"; if (name().equals(\"WATER_AMBIENT\")) return \"water_ambient\"; return \"misc\";"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/entity/MobCategory",
                "getMaxInstancesPerChunk",
                "int"
            ),
            "if (name().equals(\"MONSTER\")) return 70; if (name().equals(\"CREATURE\")) return 10; if (name().equals(\"AMBIENT\")) return 15; if (name().equals(\"WATER_AMBIENT\")) return 20; if (name().equals(\"MISC\")) return -1; return 5;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/entity/MobCategory",
                "isFriendly",
                "boolean"
            ),
            "return !name().equals(\"MONSTER\");"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/entity/MobCategory",
                "getDespawnDistance",
                "int"
            ),
            "return name().equals(\"WATER_AMBIENT\") ? 64 : 128;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/client/KeyMapping$Category",
                "register",
                "net.minecraft.client.KeyMapping.Category"
            ),
            "return new Category(p0);"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/client/Minecraft",
                "getInstance",
                "net.minecraft.client.Minecraft"
            ),
            "return new Minecraft();"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/client/KeyMapping",
                "getName",
                "java.lang.String"
            ),
            "return horizonName;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/item/Item$Properties",
                "useBlockDescriptionPrefix",
                "net.minecraft.world.item.Item.Properties"
            ),
            "return this;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/level/block/state/properties/BooleanProperty",
                "create",
                "net.minecraft.world.level.block.state.properties.BooleanProperty"
            ),
            "return new BooleanProperty();"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/network/chat/Component",
                "translatable",
                "net.minecraft.network.chat.MutableComponent"
            ),
            "net.minecraft.network.chat.MutableComponent value = new net.minecraft.network.chat.MutableComponent(); value.horizonKey = p0; value.horizonText = p0; return value;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/network/chat/Component",
                "literal",
                "net.minecraft.network.chat.MutableComponent"
            ),
            "net.minecraft.network.chat.MutableComponent value = new net.minecraft.network.chat.MutableComponent(); value.horizonText = p0; return value;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/network/chat/MutableComponent",
                "getString",
                "java.lang.String"
            ),
            "return horizonText;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/network/chat/MutableComponent",
                "plainCopy",
                "net.minecraft.network.chat.MutableComponent"
            ),
            "net.minecraft.network.chat.MutableComponent value = new net.minecraft.network.chat.MutableComponent(); value.horizonText = horizonText; return value;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/network/chat/MutableComponent",
                "withStyle",
                "net.minecraft.network.chat.MutableComponent"
            ),
            "return this;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/core/Vec3i",
                "offsetCodec",
                "com.mojang.serialization.Codec"
            ),
            "return com.mojang.serialization.Codec.STRING;"
        );
        assert_eq!(
            runtime_body("net/minecraft/core/Vec3i", "getX", "int"),
            "return horizonX;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/core/Direction",
                "getOpposite",
                "net.minecraft.core.Direction"
            ),
            "return switch (this) { case DOWN -> UP; case UP -> DOWN; case NORTH -> SOUTH; case SOUTH -> NORTH; case WEST -> EAST; case EAST -> WEST; };"
        );
        assert_eq!(
            runtime_body("net/minecraft/core/Direction", "getStepX", "int"),
            "return this == WEST ? -1 : this == EAST ? 1 : 0;"
        );
        assert_eq!(
            runtime_body("net/minecraft/core/Direction$Axis", "isVertical", "boolean"),
            "return this == Y;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/InteractionHand",
                "getName",
                "java.lang.String"
            ),
            "return this == MAIN_HAND ? \"main_hand\" : \"off_hand\";"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/InteractionHand",
                "getOpposite",
                "net.minecraft.world.InteractionHand"
            ),
            "return this == MAIN_HAND ? OFF_HAND : MAIN_HAND;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/core/BlockPos",
                "above",
                "net.minecraft.core.BlockPos"
            ),
            "return new net.minecraft.core.BlockPos(horizonX, horizonY + 1, horizonZ);"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/level/levelgen/structure/placement/StructurePlacement",
                "codec",
                "com.mojang.serialization.MapCodec"
            ),
            "return com.mojang.serialization.Codec.STRING.fieldOf(\"\");"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/level/levelgen/structure/placement/StructurePlacement",
                "codec",
                "com.mojang.serialization.Codec"
            ),
            "return com.mojang.serialization.Codec.STRING;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/resources/FileToIdConverter",
                "json",
                "net.minecraft.resources.FileToIdConverter"
            ),
            "return new FileToIdConverter();"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/network/protocol/common/custom/CustomPacketPayload$Type",
                "id",
                "net.minecraft.resources.Identifier"
            ),
            "return horizonId;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/network/protocol/common/custom/CustomPacketPayload$Type",
                "toString",
                "java.lang.String",
                "()Ljava/lang/String;",
            ),
            "return horizonId.toString();"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/network/protocol/common/custom/CustomPacketPayload$Type",
                "equals",
                "boolean",
                "(Ljava/lang/Object;)Z",
            ),
            "return horizonId.equals(p0);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/network/protocol/common/custom/CustomPacketPayload$Type",
                "hashCode",
                "int",
                "()I",
            ),
            "return horizonId.hashCode();"
        );
        // A buffer-retyping factory keeps its receiver: the bounded codec
        // answers for the value it already holds.
        assert_eq!(
            stream_codec_body("cast", "()Lnet/minecraft/network/codec/StreamCodec;").as_deref(),
            Some("return this;")
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/item/CreativeModeTab$Builder",
                "title",
                "net.minecraft.world.item.CreativeModeTab.Builder"
            ),
            "return this;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/level/block/Block",
                "asItem",
                "net.minecraft.world.item.Item"
            ),
            "return new net.minecraft.world.item.Item();"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/entity/EntityType$Builder",
                "of",
                "net.minecraft.world.entity.EntityType.Builder"
            ),
            "return new EntityType.Builder();"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/entity/EntityType$Builder",
                "sized",
                "net.minecraft.world.entity.EntityType.Builder"
            ),
            "horizonWidth = p0; horizonHeight = p1; return this;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/entity/EntityType$Builder",
                "fireImmune",
                "net.minecraft.world.entity.EntityType.Builder"
            ),
            "horizonFireImmune = true; return this;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/entity/EntityType$Builder",
                "build",
                "net.minecraft.world.entity.EntityType"
            ),
            "net.minecraft.world.entity.EntityType value = new net.minecraft.world.entity.EntityType(horizonWidth, horizonHeight, horizonTrackingRange, horizonUpdateInterval, horizonFireImmune, horizonCategory); value.horizonCanSpawnFarFromPlayer = horizonCanSpawnFarFromPlayer; value.horizonCanSummon = horizonCanSummon; value.horizonSaves = horizonSaves; return value;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/entity/EntityType",
                "getTrackingRange",
                "int"
            ),
            "return horizonTrackingRange;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/level/block/Block",
                "column",
                "net.minecraft.world.phys.shapes.VoxelShape"
            ),
            voxel_shape_placeholder()
        );
        assert!(voxel_shape_placeholder().contains("DoubleLists.EMPTY_LIST"));
    }

    #[test]
    fn emitted_methods_keep_game_visibility() {
        let class = Class {
            name: "net/minecraft/Test".to_owned(),
            declared: vec![Declared {
                name: "value".to_owned(),
                descriptor: "()V".to_owned(),
                access: 0x0001,
                kind: MemberKind::Method,
            }],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([("value".to_owned(), "()V".to_owned())])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("public void value()"));
    }

    #[test]
    fn item_stack_components_keep_common_typed_operations() {
        let component = "Lnet/minecraft/core/component/DataComponentType;";
        let object = "Ljava/lang/Object;";
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/item/ItemStack",
                "get",
                "java.lang.Object",
                &format!("({component}){object}"),
            ),
            "return horizonComponents.get(p0);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/item/ItemStack",
                "getOrDefault",
                "java.lang.Object",
                &format!("({component}{object}){object}"),
            ),
            "Object value = horizonComponents.get(p0); return value == null ? p1 : value;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/item/ItemStack",
                "set",
                "java.lang.Object",
                &format!("({component}{object}){object}"),
            ),
            "return horizonComponents.put(p0, p1);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/item/ItemStack",
                "remove",
                "java.lang.Object",
                &format!("({component}){object}"),
            ),
            "return horizonComponents.remove(p0);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/item/ItemStack",
                "has",
                "boolean",
                &format!("({component})Z"),
            ),
            "return horizonComponents.containsKey(p0);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/item/ItemStack",
                "hasNonDefault",
                "boolean",
                &format!("({component})Z"),
            ),
            "return horizonComponents.containsKey(p0);"
        );
    }

    #[test]
    fn item_properties_preserve_stack_and_durability_contract() {
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/item/Item$Properties",
                "stacksTo",
                "net.minecraft.world.item.Item.Properties",
                "(I)Lnet/minecraft/world/item/Item$Properties;",
            ),
            "horizonMaxStackSize = Math.max(1, Math.min(99, p0)); return this;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/item/Item$Properties",
                "durability",
                "net.minecraft.world.item.Item.Properties",
                "(I)Lnet/minecraft/world/item/Item$Properties;",
            ),
            "horizonMaxDamage = Math.max(0, p0); horizonMaxStackSize = 1; return this;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/item/Item$Properties",
                "craftRemainder",
                "net.minecraft.world.item.Item.Properties",
                "(Lnet/minecraft/world/item/Item;)Lnet/minecraft/world/item/Item$Properties;",
            ),
            "horizonCraftRemainder = p0; return this;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/item/Item$Properties",
                "fireResistant",
                "net.minecraft.world.item.Item.Properties",
                "()Lnet/minecraft/world/item/Item$Properties;",
            ),
            "horizonFireResistant = true; return this;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/item/Item",
                "getMaxDamage",
                "int",
                "()I",
            ),
            "return horizonMaxDamage;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/item/Item",
                "isDamageable",
                "boolean",
                "()Z",
            ),
            "return horizonMaxDamage > 0;"
        );
    }

    #[test]
    fn block_properties_preserve_common_registration_values() {
        let properties = "net.minecraft.world.level.block.state.BlockBehaviour.Properties";
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/block/state/BlockBehaviour$Properties",
                "strength",
                properties,
                "(F)Lnet/minecraft/world/level/block/state/BlockBehaviour$Properties;",
            ),
            "horizonDestroyTime = Math.max(0.0f, p0); horizonExplosionResistance = Math.max(0.0f, p0); return this;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/block/state/BlockBehaviour$Properties",
                "strength",
                properties,
                "(FF)Lnet/minecraft/world/level/block/state/BlockBehaviour$Properties;",
            ),
            "horizonDestroyTime = Math.max(0.0f, p0); horizonExplosionResistance = Math.max(0.0f, p1); return this;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/block/state/BlockBehaviour$Properties",
                "noOcclusion",
                properties,
                "()Lnet/minecraft/world/level/block/state/BlockBehaviour$Properties;",
            ),
            "horizonNoOcclusion = true; return this;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/block/state/BlockBehaviour$Properties",
                "friction",
                properties,
                "(F)Lnet/minecraft/world/level/block/state/BlockBehaviour$Properties;",
            ),
            "horizonFriction = Math.max(0.0f, p0); return this;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/block/state/BlockBehaviour$Properties",
                "requiresCorrectToolForDrops",
                properties,
                "()Lnet/minecraft/world/level/block/state/BlockBehaviour$Properties;",
            ),
            "horizonCorrectTool = true; return this;"
        );
    }

    #[test]
    fn item_stack_facade_emits_component_storage_and_copy() {
        let class = Class {
            name: "net/minecraft/world/item/ItemStack".to_owned(),
            superclass: "java/lang/Object".to_owned(),
            declared: vec![
                Declared {
                    name: "get".to_owned(),
                    descriptor:
                        "(Lnet/minecraft/core/component/DataComponentType;)Ljava/lang/Object;"
                            .to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "copy".to_owned(),
                    descriptor: "()Lnet/minecraft/world/item/ItemStack;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([
                (
                    "get".to_owned(),
                    "(Lnet/minecraft/core/component/DataComponentType;)Ljava/lang/Object;"
                        .to_owned(),
                ),
                (
                    "copy".to_owned(),
                    "()Lnet/minecraft/world/item/ItemStack;".to_owned(),
                ),
            ])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("horizonComponents = new java.util.HashMap<>()"));
        assert!(source.contains("return horizonComponents.get(p0);"));
        assert!(source.contains("horizonComponents.putAll(horizonComponents)"));
    }

    #[test]
    fn vec3i_facade_preserves_coordinates_for_value_queries() {
        let class = Class {
            name: "net/minecraft/core/Vec3i".to_owned(),
            superclass: "java/lang/Object".to_owned(),
            declared: vec![
                Declared {
                    name: "<init>".to_owned(),
                    descriptor: "(III)V".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getX".to_owned(),
                    descriptor: "()I".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([("getX".to_owned(), "()I".to_owned())])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("protected int horizonX;"));
        assert!(source.contains("this.horizonX = p0;"));
        assert!(source.contains("public int getX() { return horizonX; }"));
    }

    #[test]
    fn block_pos_facade_preserves_common_offset_and_distance_queries() {
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/BlockPos",
                "offset",
                "net.minecraft.core.BlockPos",
                "(III)Lnet/minecraft/core/BlockPos;",
            ),
            "return new net.minecraft.core.BlockPos(horizonX + p0, horizonY + p1, horizonZ + p2);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/BlockPos",
                "relative",
                "net.minecraft.core.BlockPos",
                "(Lnet/minecraft/core/Direction;)Lnet/minecraft/core/BlockPos;",
            ),
            "return new net.minecraft.core.BlockPos(horizonX + p0.getStepX(), horizonY + p0.getStepY(), horizonZ + p0.getStepZ());"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/BlockPos",
                "distSqr",
                "double",
                "(Lnet/minecraft/core/Vec3i;)D",
            ),
            "double dx = horizonX - p0.getX(); double dy = horizonY - p0.getY(); double dz = horizonZ - p0.getZ(); return dx * dx + dy * dy + dz * dz;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/BlockPos",
                "closerThan",
                "boolean",
                "(Lnet/minecraft/core/Vec3i;D)Z",
            ),
            "return distSqr(p0) < p1 * p1;"
        );
    }

    #[test]
    fn vec3_facade_preserves_coordinates_and_vector_arithmetic() {
        let class = Class {
            name: "net/minecraft/world/phys/Vec3".to_owned(),
            superclass: "java/lang/Object".to_owned(),
            declared: vec![
                Declared {
                    name: "<init>".to_owned(),
                    descriptor: "(DDD)V".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "x".to_owned(),
                    descriptor: "()D".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "add".to_owned(),
                    descriptor: "(Lnet/minecraft/world/phys/Vec3;)Lnet/minecraft/world/phys/Vec3;"
                        .to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([
                ("x".to_owned(), "()D".to_owned()),
                (
                    "add".to_owned(),
                    "(Lnet/minecraft/world/phys/Vec3;)Lnet/minecraft/world/phys/Vec3;".to_owned(),
                ),
            ])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("protected double horizonX;"));
        assert!(source.contains("this.horizonX = p0;"));
        assert!(source.contains("public double x() { return horizonX; }"));
        assert!(source.contains("horizonX + p0.x()"));
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/phys/Vec3",
                "scale",
                "net.minecraft.world.phys.Vec3",
                "(D)Lnet/minecraft/world/phys/Vec3;",
            ),
            "return new net.minecraft.world.phys.Vec3(horizonX * p0, horizonY * p0, horizonZ * p0);"
        );
    }

    #[test]
    fn block_state_facade_retains_property_values() {
        let class = Class {
            name: "net/minecraft/world/level/block/state/BlockState".to_owned(),
            superclass: "java/lang/Object".to_owned(),
            declared: vec![
                Declared {
                    name: "setValue".to_owned(),
                    descriptor: "(Lnet/minecraft/world/level/block/state/properties/Property;Ljava/lang/Comparable;)Lnet/minecraft/world/level/block/state/BlockState;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getValue".to_owned(),
                    descriptor: "(Lnet/minecraft/world/level/block/state/properties/Property;)Ljava/lang/Comparable;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "hasProperty".to_owned(),
                    descriptor: "(Lnet/minecraft/world/level/block/state/properties/Property;)Z".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getFluidState".to_owned(),
                    descriptor: "()Lnet/minecraft/world/level/material/FluidState;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([
                (
                    "setValue".to_owned(),
                    "(Lnet/minecraft/world/level/block/state/properties/Property;Ljava/lang/Comparable;)Lnet/minecraft/world/level/block/state/BlockState;".to_owned(),
                ),
                (
                    "getValue".to_owned(),
                    "(Lnet/minecraft/world/level/block/state/properties/Property;)Ljava/lang/Comparable;".to_owned(),
                ),
                (
                    "hasProperty".to_owned(),
                    "(Lnet/minecraft/world/level/block/state/properties/Property;)Z".to_owned(),
                ),
            ])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("net.minecraft.world.level.block.Block horizonBlock"));
        assert!(source.contains("IdentityHashMap<Object, Object> horizonValues"));
        assert!(source.contains("horizonValues.put(p0, p1); return this;"));
        assert!(source.contains("return (java.lang.Comparable) horizonValues.get(p0);"));
        assert!(source.contains("return horizonValues.containsKey(p0);"));
        assert!(source.contains("public boolean hasBlockEntity()"));
        assert!(source.contains("public boolean isRandomlyTicking()"));
        assert!(
            source.contains("public net.minecraft.world.level.material.FluidState getFluidState()")
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/block/state/BlockState",
                "getProperties",
                "java.util.Collection",
                "()Ljava/util/Collection;",
            ),
            "return horizonValues.keySet();"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/block/state/BlockState",
                "setValue",
                "net.minecraft.world.level.block.state.BlockState",
                "(Lnet/minecraft/world/level/block/state/properties/Property;Ljava/lang/Comparable;)Lnet/minecraft/world/level/block/state/BlockState;",
            ),
            "horizonValues.put(p0, p1); return this;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/block/state/BlockState",
                "getBlock",
                "net.minecraft.world.level.block.Block",
                "()Lnet/minecraft/world/level/block/Block;",
            ),
            "return horizonBlock;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/block/state/BlockState",
                "getFluidState",
                "net.minecraft.world.level.material.FluidState",
                "()Lnet/minecraft/world/level/material/FluidState;",
            ),
            "return new net.minecraft.world.level.material.FluidState();"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/block/state/BlockState",
                "is",
                "boolean",
                "(Lnet/minecraft/tags/TagKey;)Z",
            ),
            "return false;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/block/state/BlockState",
                "isAir",
                "boolean",
                "()Z",
            ),
            "return false;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/block/state/BlockState",
                "hasBlockEntity",
                "boolean",
                "()Z",
            ),
            "return false;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/block/state/BlockState",
                "isRandomlyTicking",
                "boolean",
                "()Z",
            ),
            "return false;"
        );
    }

    #[test]
    fn fluid_state_facade_preserves_empty_source_and_amount_queries() {
        let class = Class {
            name: "net/minecraft/world/level/material/FluidState".to_owned(),
            superclass: "java/lang/Object".to_owned(),
            declared: vec![
                Declared {
                    name: "isEmpty".to_owned(),
                    descriptor: "()Z".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getAmount".to_owned(),
                    descriptor: "()I".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([
                ("isEmpty".to_owned(), "()Z".to_owned()),
                ("getAmount".to_owned(), "()I".to_owned()),
            ])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("protected boolean horizonEmpty = true;"));
        assert!(source.contains("protected int horizonAmount;"));
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/material/FluidState",
                "isEmpty",
                "boolean",
                "()Z",
            ),
            "return horizonEmpty;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/material/FluidState",
                "getAmount",
                "int",
                "()I",
            ),
            "return horizonAmount;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/material/FluidState",
                "isSource",
                "boolean",
                "()Z",
            ),
            "return !horizonEmpty && horizonAmount >= 8;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/material/FluidState",
                "getHeight",
                "float",
                "()F",
            ),
            "return horizonAmount / 9.0f;"
        );
    }

    #[test]
    fn mutable_component_facade_preserves_literal_text() {
        let class = Class {
            name: "net/minecraft/network/chat/MutableComponent".to_owned(),
            superclass: "java/lang/Object".to_owned(),
            declared: vec![Declared {
                name: "getString".to_owned(),
                descriptor: "()Ljava/lang/String;".to_owned(),
                access: 0x0001,
                kind: MemberKind::Method,
            }],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([(
                "getString".to_owned(),
                "()Ljava/lang/String;".to_owned(),
            )])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("protected String horizonText = \"\";"));
        assert!(source.contains("public java.lang.String getString() { return horizonText; }"));
    }

    #[test]
    fn tag_key_facade_preserves_registry_and_location() {
        // 26.3 makes `TagKey` a record. Its components carry the two values,
        // the canonical accessors come from the record itself, and the bounded
        // `isFor` body reads the component rather than a shadow field.
        let class = Class {
            name: "net/minecraft/tags/TagKey".to_owned(),
            superclass: "java/lang/Record".to_owned(),
            declared: vec![
                Declared {
                    name: "registry".to_owned(),
                    descriptor: "Lnet/minecraft/resources/ResourceKey;".to_owned(),
                    access: 0x0012,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "location".to_owned(),
                    descriptor: "Lnet/minecraft/resources/Identifier;".to_owned(),
                    access: 0x0012,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "location".to_owned(),
                    descriptor: "()Lnet/minecraft/resources/Identifier;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "registry".to_owned(),
                    descriptor: "()Lnet/minecraft/resources/ResourceKey;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "isFor".to_owned(),
                    descriptor: "(Lnet/minecraft/resources/ResourceKey;)Z".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([
                (
                    "location".to_owned(),
                    "()Lnet/minecraft/resources/Identifier;".to_owned(),
                ),
                (
                    "registry".to_owned(),
                    "()Lnet/minecraft/resources/ResourceKey;".to_owned(),
                ),
                (
                    "isFor".to_owned(),
                    "(Lnet/minecraft/resources/ResourceKey;)Z".to_owned(),
                ),
            ])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(
            source.contains(
                "public record TagKey(net.minecraft.resources.ResourceKey registry, net.minecraft.resources.Identifier location)"
            ),
            "TagKey is not a record: {source}"
        );
        assert!(!source.contains("horizonRegistry"), "{source}");
        assert!(!source.contains("horizonLocation"), "{source}");
        // The record synthesizes both component accessors; redeclaring either
        // one is a javac error.
        assert!(
            !source.contains("public net.minecraft.resources.Identifier location()"),
            "{source}"
        );
        assert!(
            !source.contains("public net.minecraft.resources.ResourceKey registry()"),
            "{source}"
        );
        assert!(
            source.contains("public boolean isFor(net.minecraft.resources.ResourceKey p0) { return java.util.Objects.equals(registry, p0); }"),
            "{source}"
        );
        assert!(source.contains("public TagKey() { this("), "{source}");
        // `javac` is the judge: a record that redeclared a field or an
        // accessor would be rejected here, not at mod load.
        use std::process::Command;
        let root =
            std::env::temp_dir().join(format!("horizon-facade-tag-key-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        let write = |relative: &str, text: &str| {
            let file = src.join(relative);
            fs::create_dir_all(file.parent().expect("package directory")).expect("create package");
            fs::write(&file, text).expect("write the java source");
            file
        };
        let files = [
            write(
                "net/minecraft/resources/Identifier.java",
                "package net.minecraft.resources;\npublic class Identifier {}\n",
            ),
            write(
                "net/minecraft/resources/ResourceKey.java",
                "package net.minecraft.resources;\npublic class ResourceKey {}\n",
            ),
            write("net/minecraft/tags/TagKey.java", &source),
        ];
        fs::create_dir_all(&classes).expect("create classes directory");
        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let javac = jdk_bin
            .as_ref()
            .map(|bin| bin.join("javac"))
            .unwrap_or_else(|| PathBuf::from("javac"));
        let Ok(status) = Command::new(&javac)
            .arg("-nowarn")
            .arg("-d")
            .arg(&classes)
            .args(&files)
            .status()
        else {
            let _ = fs::remove_dir_all(&root);
            return;
        };
        assert!(status.success(), "javac rejected the TagKey record");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resource_key_facade_exposes_location_and_registry_identity() {
        let class = Class {
            name: "net/minecraft/resources/ResourceKey".to_owned(),
            superclass: "java/lang/Object".to_owned(),
            declared: vec![
                Declared {
                    name: "location".to_owned(),
                    descriptor: "()Lnet/minecraft/resources/Identifier;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "isFor".to_owned(),
                    descriptor: "(Lnet/minecraft/resources/ResourceKey;)Z".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([
                (
                    "location".to_owned(),
                    "()Lnet/minecraft/resources/Identifier;".to_owned(),
                ),
                (
                    "isFor".to_owned(),
                    "(Lnet/minecraft/resources/ResourceKey;)Z".to_owned(),
                ),
            ])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("horizonRegistryName"));
        assert!(source.contains("horizonIdentifier"));
        assert!(source.contains(
            "public net.minecraft.resources.Identifier location() { return horizonIdentifier; }"
        ));
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/resources/ResourceKey",
                "isFor",
                "boolean",
                "(Lnet/minecraft/resources/ResourceKey;)Z",
            ),
            "return p0 instanceof ResourceKey other && horizonRegistryName.equals(other.horizonRegistryName);"
        );
    }

    #[test]
    fn compound_tag_facade_preserves_scalar_values() {
        let class = Class {
            name: "net/minecraft/nbt/CompoundTag".to_owned(),
            superclass: "net/minecraft/nbt/Tag".to_owned(),
            declared: vec![
                Declared {
                    name: "putString".to_owned(),
                    descriptor: "(Ljava/lang/String;Ljava/lang/String;)V".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getString".to_owned(),
                    descriptor: "(Ljava/lang/String;)Ljava/lang/String;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getInt".to_owned(),
                    descriptor: "(Ljava/lang/String;)I".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "contains".to_owned(),
                    descriptor: "(Ljava/lang/String;)Z".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getAllKeys".to_owned(),
                    descriptor: "()Ljava/util/Set;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "remove".to_owned(),
                    descriptor: "(Ljava/lang/String;)Lnet/minecraft/nbt/Tag;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "copy".to_owned(),
                    descriptor: "()Lnet/minecraft/nbt/CompoundTag;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([
                (
                    "putString".to_owned(),
                    "(Ljava/lang/String;Ljava/lang/String;)V".to_owned(),
                ),
                (
                    "getString".to_owned(),
                    "(Ljava/lang/String;)Ljava/lang/String;".to_owned(),
                ),
                ("getInt".to_owned(), "(Ljava/lang/String;)I".to_owned()),
                ("contains".to_owned(), "(Ljava/lang/String;)Z".to_owned()),
                ("getAllKeys".to_owned(), "()Ljava/util/Set;".to_owned()),
                (
                    "remove".to_owned(),
                    "(Ljava/lang/String;)Lnet/minecraft/nbt/Tag;".to_owned(),
                ),
                (
                    "copy".to_owned(),
                    "()Lnet/minecraft/nbt/CompoundTag;".to_owned(),
                ),
            ])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("protected java.util.Map<String, Object> horizonValues"));
        assert!(source.contains("horizonValues.put(p0, p1);"));
        assert!(source.contains("horizonValues.containsKey(p0)"));
        assert!(source.contains("horizonValues.remove(p0);"));
        assert!(source.contains("value.horizonValues.putAll(horizonValues)"));
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/nbt/CompoundTag",
                "put",
                "net.minecraft.nbt.Tag",
                "(Ljava/lang/String;Lnet/minecraft/nbt/Tag;)Lnet/minecraft/nbt/Tag;",
            ),
            "Object previous = horizonValues.put(p0, p1); return previous instanceof net.minecraft.nbt.Tag ? (net.minecraft.nbt.Tag) previous : null;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/nbt/CompoundTag",
                "getCompound",
                "net.minecraft.nbt.CompoundTag",
                "(Ljava/lang/String;)Lnet/minecraft/nbt/CompoundTag;",
            ),
            "Object value = horizonValues.get(p0); return value instanceof net.minecraft.nbt.CompoundTag ? (net.minecraft.nbt.CompoundTag) value : new net.minecraft.nbt.CompoundTag();"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/nbt/CompoundTag",
                "isEmpty",
                "boolean",
                "()Z",
            ),
            "return horizonValues.isEmpty();"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/nbt/CompoundTag",
                "getString",
                "java.lang.String",
                "(Ljava/lang/String;)Ljava/lang/String;",
            ),
            "return horizonValues.get(p0) instanceof String ? (String) horizonValues.get(p0) : \"\";"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/nbt/CompoundTag",
                "getInt",
                "int",
                "(Ljava/lang/String;)I",
            ),
            "return horizonValues.get(p0) instanceof Integer ? ((Integer) horizonValues.get(p0)).intValue() : 0;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/nbt/CompoundTag",
                "getAllKeys",
                "java.util.Set",
                "()Ljava/util/Set;",
            ),
            "return horizonValues.keySet();"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/nbt/CompoundTag",
                "remove",
                "net.minecraft.nbt.Tag",
                "(Ljava/lang/String;)Lnet/minecraft/nbt/Tag;",
            ),
            "Object previous = horizonValues.remove(p0); return previous instanceof net.minecraft.nbt.Tag ? (net.minecraft.nbt.Tag) previous : null;"
        );
    }

    #[test]
    fn compound_and_list_tags_preserve_numeric_and_list_contracts() {
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/nbt/CompoundTag",
                "putDouble",
                "void",
                "(Ljava/lang/String;D)V",
            ),
            "horizonValues.put(p0, p1);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/nbt/CompoundTag",
                "getFloat",
                "float",
                "(Ljava/lang/String;)F",
            ),
            "return horizonValues.get(p0) instanceof Number ? ((Number) horizonValues.get(p0)).floatValue() : 0.0f;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/nbt/CompoundTag",
                "putByteArray",
                "void",
                "(Ljava/lang/String;[B)V",
            ),
            "horizonValues.put(p0, p1 == null ? new byte[0] : p1.clone());"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/nbt/CompoundTag",
                "getByteArray",
                "byte[]",
                "(Ljava/lang/String;)[B",
            ),
            "Object value = horizonValues.get(p0); return value instanceof byte[] ? ((byte[]) value).clone() : new byte[0];"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/nbt/CompoundTag",
                "getList",
                "net.minecraft.nbt.ListTag",
                "(Ljava/lang/String;I)Lnet/minecraft/nbt/ListTag;",
            ),
            "Object value = horizonValues.get(p0); return value instanceof net.minecraft.nbt.ListTag ? (net.minecraft.nbt.ListTag) value : new net.minecraft.nbt.ListTag();"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/nbt/ListTag",
                "add",
                "boolean",
                "(Lnet/minecraft/nbt/Tag;)Z",
            ),
            "return horizonValues.add(p0);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/nbt/ListTag",
                "getCompound",
                "net.minecraft.nbt.CompoundTag",
                "(I)Lnet/minecraft/nbt/CompoundTag;",
            ),
            "Object value = horizonValues.get(p0); return value instanceof net.minecraft.nbt.CompoundTag ? (net.minecraft.nbt.CompoundTag) value : new net.minecraft.nbt.CompoundTag();"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/nbt/ListTag",
                "remove",
                "net.minecraft.nbt.Tag",
                "(I)Lnet/minecraft/nbt/Tag;",
            ),
            "return horizonValues.remove(p0);"
        );
        let class = Class {
            name: "net/minecraft/nbt/ListTag".to_owned(),
            superclass: "net/minecraft/nbt/Tag".to_owned(),
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::new()),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("protected java.util.List<net.minecraft.nbt.Tag> horizonValues"));
    }

    #[test]
    fn holder_reference_facade_keeps_bound_value_and_key_shape() {
        let class = Class {
            name: "net/minecraft/core/Holder$Reference".to_owned(),
            superclass: "net/minecraft/core/Holder".to_owned(),
            declared: vec![
                Declared {
                    name: "value".to_owned(),
                    descriptor: "()Ljava/lang/Object;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "unwrapKey".to_owned(),
                    descriptor: "()Ljava/util/Optional;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "key".to_owned(),
                    descriptor: "()Lnet/minecraft/resources/ResourceKey;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([
                ("value".to_owned(), "()Ljava/lang/Object;".to_owned()),
                ("unwrapKey".to_owned(), "()Ljava/util/Optional;".to_owned()),
                (
                    "key".to_owned(),
                    "()Lnet/minecraft/resources/ResourceKey;".to_owned(),
                ),
            ])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("protected Object horizonValue;"));
        assert!(source.contains("void horizonBind(Object value"));
        assert!(source.contains("return horizonValue;"));
        assert!(source.contains("return java.util.Optional.ofNullable(horizonKey);"));
        assert!(source.contains("return horizonKey;"));
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/Holder$Reference",
                "key",
                "net.minecraft.resources.ResourceKey",
                "()Lnet/minecraft/resources/ResourceKey;",
            ),
            "return horizonKey;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/Holder$Reference",
                "canSerializeIn",
                "boolean",
                "(Lnet/minecraft/core/HolderOwner;)Z",
            ),
            "return horizonValue != null;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/Registry",
                "get",
                "java.lang.Object",
                "(Lnet/minecraft/resources/ResourceKey;)Ljava/lang/Object;",
            ),
            "return (java.lang.Object) horizonEntries.get(p0.location());"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/Registry",
                "getHolder",
                "java.util.Optional",
                "(Lnet/minecraft/resources/ResourceKey;)Ljava/util/Optional;",
            ),
            "Object value = horizonEntries.get(p0.location()); if (value == null) return java.util.Optional.empty(); net.minecraft.core.Holder.Reference holder = new net.minecraft.core.Holder.Reference(); holder.horizonBind(value, p0, p0.location()); return java.util.Optional.of(holder);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/Registry",
                "getOrThrow",
                "java.lang.Object",
                "(Lnet/minecraft/resources/ResourceKey;)Ljava/lang/Object;",
            ),
            "Object value = horizonEntries.get(p0.location()); if (value == null) throw new IllegalStateException(\"missing registry value: \" + p0.location()); return (java.lang.Object) value;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/Registry",
                "registerForHolder",
                "net.minecraft.core.Holder.Reference",
                "(Lnet/minecraft/core/Registry;Lnet/minecraft/resources/ResourceKey;Lnet/minecraft/resources/Identifier;Ljava/lang/Object;)Lnet/minecraft/core/Holder$Reference;",
            ),
            "p0.horizonEntries.put(p2, p3); p0.horizonKeys.put(p2, p1); net.minecraft.core.Holder.Reference holder = new net.minecraft.core.Holder.Reference(); holder.horizonBind(p3, p1, p2); return holder;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/Holder",
                "canSerializeIn",
                "boolean",
                "(Lnet/minecraft/core/HolderOwner;)Z",
            ),
            "return false;"
        );
    }

    #[test]
    fn holder_set_facade_keeps_bounded_membership_shape() {
        assert_eq!(
            runtime_body_with_descriptor("net/minecraft/core/HolderSet", "size", "int", "()I",),
            "return 0;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/HolderSet",
                "isEmpty",
                "boolean",
                "()Z",
            ),
            "return true;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/HolderSet",
                "contains",
                "boolean",
                "(Lnet/minecraft/core/Holder;)Z",
            ),
            "return false;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/HolderSet",
                "stream",
                "java.util.stream.Stream",
                "()Ljava/util/stream/Stream;",
            ),
            "return java.util.stream.Stream.empty();"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/HolderSet",
                "direct",
                "net.minecraft.core.HolderSet",
                "(Lnet/minecraft/core/Holder;)Lnet/minecraft/core/HolderSet;",
            ),
            "return new net.minecraft.core.HolderSet() { public int size() { return 1; } public boolean isEmpty() { return false; } public boolean contains(net.minecraft.core.Holder value) { return value == p0; } public java.util.stream.Stream stream() { return java.util.stream.Stream.of(p0); } public net.minecraft.core.Holder get(int index) { return index == 0 ? p0 : null; } };"
        );
    }

    #[test]
    fn registry_resource_key_lookup_keeps_registration_identity() {
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/Registry",
                "register",
                "java.lang.Object",
                "(Lnet/minecraft/core/Registry;Lnet/minecraft/resources/ResourceKey;Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            "p0.horizonEntries.put(p1.location(), p2); p0.horizonKeys.put(p1.location(), p1); return p2;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/Registry",
                "register",
                "java.lang.Object",
                "(Lnet/minecraft/resources/ResourceKey;Lnet/minecraft/resources/Identifier;Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            "horizonEntries.put(p1, p2); horizonKeys.put(p1, p0); return p2;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/Registry",
                "getResourceKey",
                "java.util.Optional",
                "(Ljava/lang/Object;)Ljava/util/Optional;",
            ),
            "net.minecraft.resources.Identifier id = getKey(p0); return id == null ? java.util.Optional.empty() : java.util.Optional.ofNullable(horizonKeys.get(id));"
        );
    }

    #[test]
    fn level_facade_preserves_common_world_read_contract() {
        let class = Class {
            name: "net/minecraft/world/level/Level".to_owned(),
            superclass: "java/lang/Object".to_owned(),
            declared: vec![
                Declared {
                    name: "getGameTime".to_owned(),
                    descriptor: "()J".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "dimension".to_owned(),
                    descriptor: "()Lnet/minecraft/resources/ResourceKey;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getBlockState".to_owned(),
                    descriptor:
                        "(Lnet/minecraft/core/BlockPos;)Lnet/minecraft/world/level/block/state/BlockState;"
                            .to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getBlockEntity".to_owned(),
                    descriptor:
                        "(Lnet/minecraft/core/BlockPos;)Lnet/minecraft/world/level/block/entity/BlockEntity;"
                            .to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "setBlock".to_owned(),
                    descriptor:
                        "(Lnet/minecraft/core/BlockPos;Lnet/minecraft/world/level/block/state/BlockState;I)Z"
                            .to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "setBlockEntity".to_owned(),
                    descriptor:
                        "(Lnet/minecraft/world/level/block/entity/BlockEntity;)V".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "isClientSide".to_owned(),
                    descriptor: "()Z".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([
                ("getGameTime".to_owned(), "()J".to_owned()),
                (
                    "dimension".to_owned(),
                    "()Lnet/minecraft/resources/ResourceKey;".to_owned(),
                ),
                (
                    "getBlockState".to_owned(),
                    "(Lnet/minecraft/core/BlockPos;)Lnet/minecraft/world/level/block/state/BlockState;"
                        .to_owned(),
                ),
                (
                    "getBlockEntity".to_owned(),
                    "(Lnet/minecraft/core/BlockPos;)Lnet/minecraft/world/level/block/entity/BlockEntity;"
                        .to_owned(),
                ),
                (
                    "setBlock".to_owned(),
                    "(Lnet/minecraft/core/BlockPos;Lnet/minecraft/world/level/block/state/BlockState;I)Z"
                        .to_owned(),
                ),
                (
                    "setBlockEntity".to_owned(),
                    "(Lnet/minecraft/world/level/block/entity/BlockEntity;)V".to_owned(),
                ),
                ("isClientSide".to_owned(), "()Z".to_owned()),
            ])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("protected long horizonGameTime;"));
        assert!(source.contains("protected float horizonRainLevel;"));
        assert!(source.contains("protected float horizonThunderLevel;"));
        assert!(source.contains("horizonDimension"));
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/Level",
                "getGameTime",
                "long",
                "()J"
            ),
            "return horizonGameTime;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/Level",
                "getDayTime",
                "long",
                "()J"
            ),
            "return horizonGameTime;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/Level",
                "isLoaded",
                "boolean",
                "(Lnet/minecraft/core/BlockPos;)Z",
            ),
            "return horizonBlocks.containsKey(horizonBlockKey(p0)) || horizonBlockEntities.containsKey(horizonBlockKey(p0));"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/Level",
                "canSeeSky",
                "boolean",
                "(Lnet/minecraft/core/BlockPos;)Z",
            ),
            "return true;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/Level",
                "isRaining",
                "boolean",
                "()Z",
            ),
            "return horizonRainLevel > 0.0f;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/Level",
                "isRainingAt",
                "boolean",
                "(Lnet/minecraft/core/BlockPos;)Z",
            ),
            "return horizonRainLevel > 0.0f && canSeeSky(p0);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/Level",
                "getThunderLevel",
                "float",
                "(F)F",
            ),
            "return horizonThunderLevel;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/Level",
                "getBlockState",
                "net.minecraft.world.level.block.state.BlockState",
                "(Lnet/minecraft/core/BlockPos;)Lnet/minecraft/world/level/block/state/BlockState;",
            ),
            "return horizonBlocks.getOrDefault(horizonBlockKey(p0), new net.minecraft.world.level.block.state.BlockState());"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/Level",
                "setBlock",
                "boolean",
                "(Lnet/minecraft/core/BlockPos;Lnet/minecraft/world/level/block/state/BlockState;I)Z",
            ),
            "horizonBlocks.put(horizonBlockKey(p0), p1); return true;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/Level",
                "setBlockEntity",
                "void",
                "(Lnet/minecraft/world/level/block/entity/BlockEntity;)V",
            ),
            "if (p0 != null) horizonBlockEntities.put(horizonBlockKey(p0.getBlockPos()), p0);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/Level",
                "getBlockEntity",
                "net.minecraft.world.level.block.entity.BlockEntity",
                "(Lnet/minecraft/core/BlockPos;)Lnet/minecraft/world/level/block/entity/BlockEntity;",
            ),
            "return horizonBlockEntities.get(horizonBlockKey(p0));"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/server/level/ServerLevel",
                "isClientSide",
                "boolean",
                "()Z",
            ),
            "return false;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/Level",
                "getSeaLevel",
                "int",
                "()I",
            ),
            "return 63;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/Level",
                "getMaxLocalRawBrightness",
                "int",
                "(Lnet/minecraft/core/BlockPos;)I",
            ),
            "return canSeeSky(p0) ? 15 : 0;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/Level",
                "isOutsideBuildHeight",
                "boolean",
                "(Lnet/minecraft/core/BlockPos;)Z",
            ),
            "return p0.getY() < getMinBuildHeight() || p0.getY() >= getMaxBuildHeight();"
        );
    }

    #[test]
    fn block_entity_facade_preserves_position_state_and_lifecycle() {
        let class = Class {
            name: "net/minecraft/world/level/block/entity/BlockEntity".to_owned(),
            superclass: "java/lang/Object".to_owned(),
            declared: vec![
                Declared {
                    name: "getBlockPos".to_owned(),
                    descriptor: "()Lnet/minecraft/core/BlockPos;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getLevel".to_owned(),
                    descriptor: "()Lnet/minecraft/world/level/Level;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "hasLevel".to_owned(),
                    descriptor: "()Z".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getBlockState".to_owned(),
                    descriptor: "()Lnet/minecraft/world/level/block/state/BlockState;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "isRemoved".to_owned(),
                    descriptor: "()Z".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "setRemoved".to_owned(),
                    descriptor: "()V".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getUpdateTag".to_owned(),
                    descriptor: "()Lnet/minecraft/nbt/CompoundTag;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "saveAdditional".to_owned(),
                    descriptor: "(Lnet/minecraft/nbt/CompoundTag;)V".to_owned(),
                    access: 0x0004,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "load".to_owned(),
                    descriptor: "(Lnet/minecraft/nbt/CompoundTag;)V".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([
                (
                    "getBlockPos".to_owned(),
                    "()Lnet/minecraft/core/BlockPos;".to_owned(),
                ),
                (
                    "getLevel".to_owned(),
                    "()Lnet/minecraft/world/level/Level;".to_owned(),
                ),
                ("hasLevel".to_owned(), "()Z".to_owned()),
                (
                    "getBlockState".to_owned(),
                    "()Lnet/minecraft/world/level/block/state/BlockState;".to_owned(),
                ),
                ("isRemoved".to_owned(), "()Z".to_owned()),
                ("setRemoved".to_owned(), "()V".to_owned()),
                (
                    "getUpdateTag".to_owned(),
                    "()Lnet/minecraft/nbt/CompoundTag;".to_owned(),
                ),
                (
                    "saveAdditional".to_owned(),
                    "(Lnet/minecraft/nbt/CompoundTag;)V".to_owned(),
                ),
                (
                    "load".to_owned(),
                    "(Lnet/minecraft/nbt/CompoundTag;)V".to_owned(),
                ),
            ])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("horizonBlockPos"));
        assert!(source.contains("horizonBlockState"));
        assert!(source.contains("horizonData"));
        assert!(source.contains("horizonRemoved"));
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/block/entity/BlockEntity",
                "getBlockPos",
                "net.minecraft.core.BlockPos",
                "()Lnet/minecraft/core/BlockPos;",
            ),
            "return horizonBlockPos;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/block/entity/BlockEntity",
                "getBlockState",
                "net.minecraft.world.level.block.state.BlockState",
                "()Lnet/minecraft/world/level/block/state/BlockState;",
            ),
            "return horizonBlockState;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/block/entity/BlockEntity",
                "getUpdateTag",
                "net.minecraft.nbt.CompoundTag",
                "()Lnet/minecraft/nbt/CompoundTag;",
            ),
            "return horizonData.copy();"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/block/entity/BlockEntity",
                "saveAdditional",
                "void",
                "(Lnet/minecraft/nbt/CompoundTag;)V",
            ),
            "if (p0 != null) p0.merge(horizonData);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/block/entity/BlockEntity",
                "load",
                "void",
                "(Lnet/minecraft/nbt/CompoundTag;)V",
            ),
            "if (p0 != null) horizonData = p0.copy();"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/level/block/entity/BlockEntity",
                "setRemoved",
                "void",
                "()V",
            ),
            "horizonRemoved = true;"
        );
    }

    #[test]
    fn entity_facade_preserves_common_position_and_identity_contract() {
        let class = Class {
            name: "net/minecraft/world/entity/Entity".to_owned(),
            superclass: "java/lang/Object".to_owned(),
            declared: vec![
                Declared {
                    name: "getX".to_owned(),
                    descriptor: "()D".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "setPos".to_owned(),
                    descriptor: "(DDD)V".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getDeltaMovement".to_owned(),
                    descriptor: "()Lnet/minecraft/world/phys/Vec3;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "setDeltaMovement".to_owned(),
                    descriptor: "(DDD)V".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "position".to_owned(),
                    descriptor: "()Lnet/minecraft/world/phys/Vec3;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "blockPosition".to_owned(),
                    descriptor: "()Lnet/minecraft/core/BlockPos;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getEyePosition".to_owned(),
                    descriptor: "(F)Lnet/minecraft/world/phys/Vec3;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getEyeY".to_owned(),
                    descriptor: "()D".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "level".to_owned(),
                    descriptor: "()Lnet/minecraft/world/level/Level;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getUUID".to_owned(),
                    descriptor: "()Ljava/util/UUID;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getBoundingBox".to_owned(),
                    descriptor: "()Lnet/minecraft/world/phys/AABB;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "setBoundingBox".to_owned(),
                    descriptor: "(Lnet/minecraft/world/phys/AABB;)V".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "isOnGround".to_owned(),
                    descriptor: "()Z".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([
                ("getX".to_owned(), "()D".to_owned()),
                ("setPos".to_owned(), "(DDD)V".to_owned()),
                (
                    "getDeltaMovement".to_owned(),
                    "()Lnet/minecraft/world/phys/Vec3;".to_owned(),
                ),
                ("setDeltaMovement".to_owned(), "(DDD)V".to_owned()),
                (
                    "position".to_owned(),
                    "()Lnet/minecraft/world/phys/Vec3;".to_owned(),
                ),
                (
                    "blockPosition".to_owned(),
                    "()Lnet/minecraft/core/BlockPos;".to_owned(),
                ),
                (
                    "getEyePosition".to_owned(),
                    "(F)Lnet/minecraft/world/phys/Vec3;".to_owned(),
                ),
                ("getEyeY".to_owned(), "()D".to_owned()),
                (
                    "level".to_owned(),
                    "()Lnet/minecraft/world/level/Level;".to_owned(),
                ),
                ("getUUID".to_owned(), "()Ljava/util/UUID;".to_owned()),
                (
                    "getBoundingBox".to_owned(),
                    "()Lnet/minecraft/world/phys/AABB;".to_owned(),
                ),
                (
                    "setBoundingBox".to_owned(),
                    "(Lnet/minecraft/world/phys/AABB;)V".to_owned(),
                ),
                ("isOnGround".to_owned(), "()Z".to_owned()),
            ])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("protected double horizonX;"));
        assert!(source.contains("protected double horizonDeltaX;"));
        assert!(source.contains("protected net.minecraft.world.level.Level horizonLevel"));
        assert!(source.contains("horizonBoundingBox"));
        assert!(source.contains("horizonOnGround"));
        assert!(source.contains("horizonXRot"));
        assert!(source.contains("horizonRemoved"));
        assert!(source.contains("public float getEyeHeight()"));
        assert!(source.contains("public float getBbHeight()"));
        assert!(source.contains("public float getBbWidth()"));
        assert!(source.contains("public int getBlockX()"));
        assert!(source.contains("public int getBlockY()"));
        assert!(source.contains("public int getBlockZ()"));
        assert!(source.contains("public boolean isPassenger()"));
        assert!(source.contains("public net.minecraft.world.entity.Entity getVehicle()"));
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "getX",
                "double",
                "()D"
            ),
            "return horizonX;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "setPos",
                "void",
                "(DDD)V",
            ),
            "horizonX = p0; horizonY = p1; horizonZ = p2;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "setPos",
                "void",
                "(Lnet/minecraft/world/phys/Vec3;)V",
            ),
            "if (p0 != null) { horizonX = p0.x(); horizonY = p0.y(); horizonZ = p0.z(); }"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "getDeltaMovement",
                "net.minecraft.world.phys.Vec3",
                "()Lnet/minecraft/world/phys/Vec3;",
            ),
            "return new net.minecraft.world.phys.Vec3(horizonDeltaX, horizonDeltaY, horizonDeltaZ);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "setDeltaMovement",
                "void",
                "(DDD)V",
            ),
            "horizonDeltaX = p0; horizonDeltaY = p1; horizonDeltaZ = p2;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "distanceToSqr",
                "double",
                "(Lnet/minecraft/world/entity/Entity;)D",
            ),
            "double dx = horizonX - p0.getX(); double dy = horizonY - p0.getY(); double dz = horizonZ - p0.getZ(); return dx * dx + dy * dy + dz * dz;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "distanceTo",
                "float",
                "(Lnet/minecraft/world/entity/Entity;)F",
            ),
            "return (float) Math.sqrt(distanceToSqr(p0));"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "position",
                "net.minecraft.world.phys.Vec3",
                "()Lnet/minecraft/world/phys/Vec3;",
            ),
            "return new net.minecraft.world.phys.Vec3(horizonX, horizonY, horizonZ);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "blockPosition",
                "net.minecraft.core.BlockPos",
                "()Lnet/minecraft/core/BlockPos;",
            ),
            "return new net.minecraft.core.BlockPos((int) Math.floor(horizonX), (int) Math.floor(horizonY), (int) Math.floor(horizonZ));"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "getEyePosition",
                "net.minecraft.world.phys.Vec3",
                "(F)Lnet/minecraft/world/phys/Vec3;",
            ),
            "return new net.minecraft.world.phys.Vec3(horizonX, horizonY + horizonEyeHeight, horizonZ);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "getEyeY",
                "double",
                "()D",
            ),
            "return horizonY + horizonEyeHeight;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "getEyeHeight",
                "float",
                "()F",
            ),
            "return horizonEyeHeight;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "getBbHeight",
                "float",
                "()F",
            ),
            "return horizonBbHeight;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "getBbWidth",
                "float",
                "()F",
            ),
            "return horizonBbWidth;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "getBlockX",
                "int",
                "()I",
            ),
            "return (int) Math.floor(horizonX);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "getBlockY",
                "int",
                "()I",
            ),
            "return (int) Math.floor(horizonY);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "getBlockZ",
                "int",
                "()I",
            ),
            "return (int) Math.floor(horizonZ);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "isPassenger",
                "boolean",
                "()Z",
            ),
            "return false;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "getVehicle",
                "net.minecraft.world.entity.Entity",
                "()Lnet/minecraft/world/entity/Entity;",
            ),
            "return null;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "getUUID",
                "java.util.UUID",
                "()Ljava/util/UUID;",
            ),
            "return horizonUuid;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "getBoundingBox",
                "net.minecraft.world.phys.AABB",
                "()Lnet/minecraft/world/phys/AABB;",
            ),
            "return horizonBoundingBox;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "setBoundingBox",
                "void",
                "(Lnet/minecraft/world/phys/AABB;)V",
            ),
            "horizonBoundingBox = p0;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "isOnGround",
                "boolean",
                "()Z",
            ),
            "return horizonOnGround;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "setXRot",
                "void",
                "(F)V",
            ),
            "horizonXRot = p0;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "getYRot",
                "float",
                "()F",
            ),
            "return horizonYRot;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "isRemoved",
                "boolean",
                "()Z",
            ),
            "return horizonRemoved;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "discard",
                "void",
                "()V",
            ),
            "horizonRemoved = true;"
        );
    }

    #[test]
    fn mob_facade_preserves_ai_flags_and_target() {
        let class = Class {
            name: "net/minecraft/world/entity/Mob".to_owned(),
            superclass: "net/minecraft/world/entity/LivingEntity".to_owned(),
            declared: vec![
                Declared {
                    name: "isNoAi".to_owned(),
                    descriptor: "()Z".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "setNoAi".to_owned(),
                    descriptor: "(Z)V".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "isAggressive".to_owned(),
                    descriptor: "()Z".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getTarget".to_owned(),
                    descriptor: "()Lnet/minecraft/world/entity/LivingEntity;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([
                ("isNoAi".to_owned(), "()Z".to_owned()),
                ("setNoAi".to_owned(), "(Z)V".to_owned()),
                ("isAggressive".to_owned(), "()Z".to_owned()),
                (
                    "getTarget".to_owned(),
                    "()Lnet/minecraft/world/entity/LivingEntity;".to_owned(),
                ),
            ])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("horizonNoAi"));
        assert!(source.contains("horizonAggressive"));
        assert!(source.contains("horizonTarget"));
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Mob",
                "isNoAi",
                "boolean",
                "()Z"
            ),
            "return horizonNoAi;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Mob",
                "setNoAi",
                "void",
                "(Z)V"
            ),
            "horizonNoAi = p0;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Mob",
                "getTarget",
                "net.minecraft.world.entity.LivingEntity",
                "()Lnet/minecraft/world/entity/LivingEntity;",
            ),
            "return horizonTarget;"
        );
    }

    #[test]
    fn entity_and_mob_facades_preserve_dimensions_and_lifecycle_flags() {
        let entity = Class {
            name: "net/minecraft/world/entity/Entity".to_owned(),
            declared: vec![
                Declared {
                    name: "getBbWidth".to_owned(),
                    descriptor: "()F".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getEyePosition".to_owned(),
                    descriptor: "()Lnet/minecraft/world/phys/Vec3;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getBlockX".to_owned(),
                    descriptor: "()I".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &entity,
            Some(&BTreeSet::from([
                ("getBbWidth".to_owned(), "()F".to_owned()),
                (
                    "getEyePosition".to_owned(),
                    "()Lnet/minecraft/world/phys/Vec3;".to_owned(),
                ),
                ("getBlockX".to_owned(), "()I".to_owned()),
            ])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("horizonBbWidth"));
        assert!(source.contains("horizonEyeHeight"));
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "getEyePosition",
                "net.minecraft.world.phys.Vec3",
                "()Lnet/minecraft/world/phys/Vec3;",
            ),
            "return new net.minecraft.world.phys.Vec3(horizonX, horizonY + horizonEyeHeight, horizonZ);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "getBlockX",
                "int",
                "()I",
            ),
            "return (int) Math.floor(horizonX);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Mob",
                "isBaby",
                "boolean",
                "()Z",
            ),
            "return horizonBaby;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Mob",
                "setPersistenceRequired",
                "void",
                "()V",
            ),
            "horizonPersistenceRequired = true;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/EntityType$Builder",
                "of",
                "net.minecraft.world.entity.EntityType.Builder",
                "(Ljava/util/function/BiFunction;Lnet/minecraft/world/entity/MobCategory;)Lnet/minecraft/world/entity/EntityType$Builder;",
            ),
            "EntityType.Builder builder = new EntityType.Builder(); builder.horizonCategory = p1; return builder;"
        );
    }

    #[test]
    fn entity_type_builder_carries_bounded_spawn_flags() {
        let class = "net/minecraft/world/entity/EntityType$Builder";
        let builder = "net.minecraft.world.entity.EntityType.Builder";
        let fluent = "()Lnet/minecraft/world/entity/EntityType$Builder;";
        for (method, expected) in [
            ("noSummon", "horizonCanSummon = false; return this;"),
            ("noSave", "horizonSaves = false; return this;"),
            (
                "canSpawnFarFromPlayer",
                "horizonCanSpawnFarFromPlayer = true; return this;",
            ),
            // No-op fluent setters keep the chain alive.
            ("immuneTo", "return this;"),
            ("requiredFeatures", "return this;"),
        ] {
            assert_eq!(
                runtime_body_with_descriptor(class, method, builder, fluent),
                expected
            );
        }
        assert_eq!(
            runtime_body_with_descriptor(
                class,
                "sized",
                builder,
                "(Lnet/minecraft/world/entity/EntityDimensions;)Lnet/minecraft/world/entity/EntityType$Builder;",
            ),
            "if (p0 != null) { horizonWidth = p0.getWidth(); horizonHeight = p0.getHeight(); } return this;"
        );
        // Both `build` overloads (a name, a key) produce the bounded value with
        // the flags the builder recorded.
        let build = "net.minecraft.world.entity.EntityType value = new net.minecraft.world.entity.EntityType(horizonWidth, horizonHeight, horizonTrackingRange, horizonUpdateInterval, horizonFireImmune, horizonCategory); value.horizonCanSpawnFarFromPlayer = horizonCanSpawnFarFromPlayer; value.horizonCanSummon = horizonCanSummon; value.horizonSaves = horizonSaves; return value;";
        for descriptor in [
            "(Ljava/lang/String;)Lnet/minecraft/world/entity/EntityType;",
            "(Lnet/minecraft/resources/ResourceKey;)Lnet/minecraft/world/entity/EntityType;",
        ] {
            assert_eq!(
                runtime_body_with_descriptor(
                    class,
                    "build",
                    "net.minecraft.world.entity.EntityType",
                    descriptor,
                ),
                build
            );
        }
        let shape = Class {
            name: class.to_owned(),
            ..Class::default()
        };
        let source = render(
            &shape,
            None,
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut Report::default(),
            true,
            &[],
        );
        assert!(source.contains("public static class Builder {"));
        for field in [
            "protected boolean horizonCanSpawnFarFromPlayer;",
            "protected boolean horizonCanSummon = true;",
            "protected boolean horizonSaves = true;",
        ] {
            assert!(source.contains(field), "Builder is missing {field}");
        }
    }

    #[test]
    fn entity_type_exposes_bounded_identity_and_dimensions() {
        let class = "net/minecraft/world/entity/EntityType";
        for (method, ret, descriptor, expected) in [
            (
                "getDimensions",
                "net.minecraft.world.entity.EntityDimensions",
                "()Lnet/minecraft/world/entity/EntityDimensions;",
                "return net.minecraft.world.entity.EntityDimensions.scalable(horizonWidth, horizonHeight);",
            ),
            (
                "getDescriptionId",
                "java.lang.String",
                "()Ljava/lang/String;",
                "return horizonDescriptionId;",
            ),
            (
                "canSpawnFarFromPlayer",
                "boolean",
                "()Z",
                "return horizonCanSpawnFarFromPlayer;",
            ),
            ("canSummon", "boolean", "()Z", "return horizonCanSummon;"),
            (
                "isEnabled",
                "boolean",
                "(Lnet/minecraft/world/flag/FeatureFlagSet;)Z",
                "return true;",
            ),
            (
                "getDescription",
                "net.minecraft.network.chat.Component",
                "()Lnet/minecraft/network/chat/Component;",
                "return net.minecraft.network.chat.Component.literal(horizonDescriptionId);",
            ),
            (
                "getDefaultLootTable",
                "java.lang.String",
                "()Ljava/lang/String;",
                "return \"minecraft:empty\";",
            ),
            (
                "getDefaultLootTable",
                "net.minecraft.resources.ResourceKey",
                "()Lnet/minecraft/resources/ResourceKey;",
                "return new net.minecraft.resources.ResourceKey();",
            ),
            (
                "getRegistryName",
                "net.minecraft.resources.Identifier",
                "()Lnet/minecraft/resources/Identifier;",
                "return new net.minecraft.resources.Identifier(\"minecraft\", \"unknown\");",
            ),
        ] {
            assert_eq!(
                runtime_body_with_descriptor(class, method, ret, descriptor),
                expected
            );
        }
        let shape = Class {
            name: class.to_owned(),
            ..Class::default()
        };
        let source = render(
            &shape,
            None,
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut Report::default(),
            false,
            &[],
        );
        for field in [
            "protected boolean horizonCanSpawnFarFromPlayer;",
            "protected boolean horizonCanSummon = true;",
            "protected boolean horizonSaves = true;",
            "protected String horizonDescriptionId = \"entity.minecraft.unknown\";",
        ] {
            assert!(source.contains(field), "EntityType is missing {field}");
        }
    }

    #[test]
    fn mob_category_and_entity_dimensions_are_bounded_values() {
        assert_eq!(
            runtime_body(
                "net/minecraft/world/entity/MobCategory",
                "getNoDespawnDistance",
                "int"
            ),
            "return 32;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/MobCategory",
                "byName",
                "net.minecraft.world.entity.MobCategory",
                "(Ljava/lang/String;)Lnet/minecraft/world/entity/MobCategory;",
            ),
            "if (p0 == null) return net.minecraft.world.entity.MobCategory.MISC; if (p0.equals(\"monster\")) return net.minecraft.world.entity.MobCategory.MONSTER; if (p0.equals(\"creature\")) return net.minecraft.world.entity.MobCategory.CREATURE; if (p0.equals(\"ambient\")) return net.minecraft.world.entity.MobCategory.AMBIENT; if (p0.equals(\"axolotls\")) return net.minecraft.world.entity.MobCategory.AXOLOTLS; if (p0.equals(\"underground_water_creature\")) return net.minecraft.world.entity.MobCategory.UNDERGROUND_WATER_CREATURE; if (p0.equals(\"water_creature\")) return net.minecraft.world.entity.MobCategory.WATER_CREATURE; if (p0.equals(\"water_ambient\")) return net.minecraft.world.entity.MobCategory.WATER_AMBIENT; return net.minecraft.world.entity.MobCategory.MISC;"
        );
        // The real class is a record; the façade overrides the shape so the value
        // keeps the accessors and backing fields a snapshotted mod names.
        let mut report = Report::default();
        let shape = Class {
            name: "net/minecraft/world/entity/EntityDimensions".to_owned(),
            superclass: "java/lang/Record".to_owned(),
            ..Class::default()
        };
        let source = render(
            &shape,
            None,
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.starts_with("package net.minecraft.world.entity;"));
        assert!(source.contains("public class EntityDimensions {"));
        for fragment in [
            "protected float width;",
            "protected float height;",
            "protected float eyeHeight;",
            "public static net.minecraft.world.entity.EntityDimensions scalable(float width, float height)",
            "public static net.minecraft.world.entity.EntityDimensions fixed(float width, float height)",
            "public net.minecraft.world.entity.EntityDimensions scale(float factor)",
            "public net.minecraft.world.entity.EntityDimensions withEyeHeight(float eyeHeight)",
            "public net.minecraft.world.phys.AABB makeBoundingBox(double x, double y, double z)",
            "public net.minecraft.world.phys.AABB makeBoundingBox(net.minecraft.world.phys.Vec3 pos)",
            "public float width() {",
            "public float height() {",
            "public float eyeHeight() {",
            "public float getWidth() {",
            "public float getHeight() {",
            "public float getEyeHeight() {",
        ] {
            assert!(
                source.contains(fragment),
                "EntityDimensions is missing {fragment}"
            );
        }
        assert_eq!(report.fields, 4);
    }

    #[test]
    fn spawn_placements_share_bounded_registration_state() {
        let class = "net/minecraft/world/entity/SpawnPlacements";
        let register = "if (p0 != null) { horizonPlacements.put(p0, p1); horizonHeightmaps.put(p0, p2); horizonSpawnRules.put(p0, Boolean.TRUE); }";
        for descriptor in [
            "(Lnet/minecraft/world/entity/EntityType;Lnet/minecraft/world/entity/SpawnPlacementType;Lnet/minecraft/world/level/levelgen/Heightmap$Types;Lnet/minecraft/world/entity/SpawnPlacements$SpawnPredicate;)V",
            // A snapshot with a height window still maps the descriptor's slots.
            "(Lnet/minecraft/world/entity/EntityType;Lnet/minecraft/world/entity/SpawnPlacementType;Lnet/minecraft/world/level/levelgen/Heightmap$Types;IILnet/minecraft/world/entity/SpawnPlacements$SpawnPredicate;)V",
        ] {
            assert_eq!(
                runtime_body_with_descriptor(class, "register", "void", descriptor),
                register
            );
        }
        for (method, ret, descriptor, expected) in [
            (
                "getPlacementType",
                "net.minecraft.world.entity.SpawnPlacementType",
                "(Lnet/minecraft/world/entity/EntityType;)Lnet/minecraft/world/entity/SpawnPlacementType;",
                "net.minecraft.world.entity.SpawnPlacementType value = horizonPlacements.get(p0); return value == null ? net.minecraft.world.entity.SpawnPlacementTypes.NO_RESTRICTIONS : value;",
            ),
            (
                "getHeightmapType",
                "net.minecraft.world.level.levelgen.Heightmap.Types",
                "(Lnet/minecraft/world/entity/EntityType;)Lnet/minecraft/world/level/levelgen/Heightmap$Types;",
                "net.minecraft.world.level.levelgen.Heightmap.Types value = horizonHeightmaps.get(p0); return value == null ? net.minecraft.world.level.levelgen.Heightmap.Types.MOTION_BLOCKING_NO_LEAVES : value;",
            ),
            (
                "isSpawnPositionOk",
                "boolean",
                "(Lnet/minecraft/world/entity/EntityType;Lnet/minecraft/world/level/LevelReader;Lnet/minecraft/core/BlockPos;)Z",
                "net.minecraft.world.entity.SpawnPlacementType value = horizonPlacements.get(p0); return value == null || value.isSpawnPositionOk(p1, p2, p0);",
            ),
            (
                "checkSpawnRules",
                "boolean",
                "(Lnet/minecraft/world/entity/EntityType;Lnet/minecraft/world/level/LevelAccessor;Lnet/minecraft/world/entity/EntitySpawnReason;Lnet/minecraft/core/BlockPos;Lnet/minecraft/util/RandomSource;)Z",
                "return horizonSpawnRules.getOrDefault(p0, Boolean.TRUE);",
            ),
            (
                "checkSpawnObstruction",
                "boolean",
                "(Lnet/minecraft/world/level/LevelReader;Lnet/minecraft/world/entity/Entity;)Z",
                "return true;",
            ),
        ] {
            assert_eq!(
                runtime_body_with_descriptor(class, method, ret, descriptor),
                expected
            );
        }
        let mut report = Report::default();
        let source = render(
            &Class {
                name: class.to_owned(),
                ..Class::default()
            },
            None,
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        for map in [
            "protected static final java.util.Map<Object, net.minecraft.world.entity.SpawnPlacementType> horizonPlacements",
            "protected static final java.util.Map<Object, net.minecraft.world.level.levelgen.Heightmap.Types> horizonHeightmaps",
            "protected static final java.util.Map<Object, Boolean> horizonSpawnRules",
        ] {
            assert!(source.contains(map), "SpawnPlacements is missing {map}");
        }
        // The registration predicate stays a SAM interface, or a mod's lambda
        // has no abstract method to bind at runtime.
        let predicate = Class {
            name: "net/minecraft/world/entity/SpawnPlacements$SpawnPredicate".to_owned(),
            access: 0x0201,
            declared: vec![Declared {
                name: "test".to_owned(),
                descriptor: "(Lnet/minecraft/world/entity/EntityType;Lnet/minecraft/world/level/LevelReader;Lnet/minecraft/world/entity/EntitySpawnReason;Lnet/minecraft/core/BlockPos;Lnet/minecraft/util/RandomSource;)Z".to_owned(),
                access: 0x0401,
                kind: MemberKind::Method,
            }],
            ..Class::default()
        };
        let predicate = render(
            &predicate,
            None,
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            true,
            &[],
        );
        assert!(predicate.contains("public static interface SpawnPredicate"));
        assert!(predicate.contains("public boolean test(net.minecraft.world.entity.EntityType p0, net.minecraft.world.level.LevelReader p1, net.minecraft.world.entity.EntitySpawnReason p2, net.minecraft.core.BlockPos p3, net.minecraft.util.RandomSource p4);"));
    }

    #[test]
    fn living_entity_facade_preserves_health_and_hand_values() {
        let class = Class {
            name: "net/minecraft/world/entity/LivingEntity".to_owned(),
            superclass: "net/minecraft/world/entity/Entity".to_owned(),
            declared: vec![
                Declared {
                    name: "getHealth".to_owned(),
                    descriptor: "()F".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getMaxHealth".to_owned(),
                    descriptor: "()F".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getMainHandItem".to_owned(),
                    descriptor: "()Lnet/minecraft/world/item/ItemStack;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "isDeadOrDying".to_owned(),
                    descriptor: "()Z".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "hasEffect".to_owned(),
                    descriptor: "(Lnet/minecraft/world/effect/MobEffect;)Z".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getEffect".to_owned(),
                    descriptor: "(Lnet/minecraft/world/effect/MobEffect;)Lnet/minecraft/world/effect/MobEffectInstance;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "removeEffect".to_owned(),
                    descriptor: "(Lnet/minecraft/world/effect/MobEffect;)Z".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([
                ("getHealth".to_owned(), "()F".to_owned()),
                ("getMaxHealth".to_owned(), "()F".to_owned()),
                (
                    "getMainHandItem".to_owned(),
                    "()Lnet/minecraft/world/item/ItemStack;".to_owned(),
                ),
                ("isDeadOrDying".to_owned(), "()Z".to_owned()),
                ("hasEffect".to_owned(), "(Lnet/minecraft/world/effect/MobEffect;)Z".to_owned()),
                ("getEffect".to_owned(), "(Lnet/minecraft/world/effect/MobEffect;)Lnet/minecraft/world/effect/MobEffectInstance;".to_owned()),
                ("removeEffect".to_owned(), "(Lnet/minecraft/world/effect/MobEffect;)Z".to_owned()),
            ])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("protected float horizonHealth = 20.0f;"));
        assert!(source.contains("horizonMainHand"));
        assert!(source.contains("horizonEffects"));
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/LivingEntity",
                "getHealth",
                "float",
                "()F",
            ),
            "return horizonHealth;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/LivingEntity",
                "setHealth",
                "void",
                "(F)V",
            ),
            "horizonHealth = p0;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/LivingEntity",
                "heal",
                "void",
                "(F)V",
            ),
            "horizonHealth = Math.min(horizonMaxHealth, horizonHealth + p0);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/LivingEntity",
                "getMainHandItem",
                "net.minecraft.world.item.ItemStack",
                "()Lnet/minecraft/world/item/ItemStack;",
            ),
            "return horizonMainHand;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/LivingEntity",
                "getItemInHand",
                "net.minecraft.world.item.ItemStack",
                "(Lnet/minecraft/world/InteractionHand;)Lnet/minecraft/world/item/ItemStack;",
            ),
            "return p0 == net.minecraft.world.InteractionHand.OFF_HAND ? horizonOffhand : horizonMainHand;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/LivingEntity",
                "addEffect",
                "boolean",
                "(Lnet/minecraft/world/effect/MobEffectInstance;)Z",
            ),
            "if (p0 == null) return false; horizonEffects.put(p0.getEffect(), p0); return true;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/LivingEntity",
                "isDeadOrDying",
                "boolean",
                "()Z",
            ),
            "return horizonHealth <= 0.0f;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/LivingEntity",
                "hasEffect",
                "boolean",
                "(Lnet/minecraft/world/effect/MobEffect;)Z",
            ),
            "return horizonEffects.containsKey(p0);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/LivingEntity",
                "getEffect",
                "net.minecraft.world.effect.MobEffectInstance",
                "(Lnet/minecraft/world/effect/MobEffect;)Lnet/minecraft/world/effect/MobEffectInstance;",
            ),
            "return horizonEffects.get(p0);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/LivingEntity",
                "removeEffect",
                "boolean",
                "(Lnet/minecraft/world/effect/MobEffect;)Z",
            ),
            "return horizonEffects.remove(p0) != null;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/LivingEntity",
                "getArmorValue",
                "int",
                "()I",
            ),
            "return horizonArmorValue;"
        );
    }

    #[test]
    fn mob_effect_instance_facade_preserves_duration_and_flags() {
        let class = Class {
            name: "net/minecraft/world/effect/MobEffectInstance".to_owned(),
            declared: vec![
                Declared {
                    name: "getDuration".to_owned(),
                    descriptor: "()I".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getAmplifier".to_owned(),
                    descriptor: "()I".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "isAmbient".to_owned(),
                    descriptor: "()Z".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "isInfiniteDuration".to_owned(),
                    descriptor: "()Z".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([
                ("getDuration".to_owned(), "()I".to_owned()),
                ("getAmplifier".to_owned(), "()I".to_owned()),
                ("isAmbient".to_owned(), "()Z".to_owned()),
                ("isInfiniteDuration".to_owned(), "()Z".to_owned()),
            ])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("horizonDuration"));
        assert!(source.contains("horizonAmplifier"));
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/effect/MobEffectInstance",
                "getDuration",
                "int",
                "()I",
            ),
            "return horizonDuration;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/effect/MobEffectInstance",
                "isInfiniteDuration",
                "boolean",
                "()Z",
            ),
            "return horizonDuration == -1;"
        );
    }

    #[test]
    fn player_facade_preserves_identity_and_access_state() {
        let class = Class {
            name: "net/minecraft/world/entity/player/Player".to_owned(),
            superclass: "net/minecraft/world/entity/LivingEntity".to_owned(),
            declared: vec![
                Declared {
                    name: "isCreative".to_owned(),
                    descriptor: "()Z".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "isSpectator".to_owned(),
                    descriptor: "()Z".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getScoreboardName".to_owned(),
                    descriptor: "()Ljava/lang/String;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getExperienceLevel".to_owned(),
                    descriptor: "()I".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getExperienceProgress".to_owned(),
                    descriptor: "()F".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([
                ("isCreative".to_owned(), "()Z".to_owned()),
                ("isSpectator".to_owned(), "()Z".to_owned()),
                (
                    "getScoreboardName".to_owned(),
                    "()Ljava/lang/String;".to_owned(),
                ),
                ("getExperienceLevel".to_owned(), "()I".to_owned()),
                ("getExperienceProgress".to_owned(), "()F".to_owned()),
            ])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("protected boolean horizonCreative;"));
        assert!(source.contains("horizonScoreboardName = \"Player\";"));
        assert!(
            source
                .contains("horizonInventory = new net.minecraft.world.entity.player.Inventory();")
        );
        assert!(
            source
                .contains("horizonAbilities = new net.minecraft.world.entity.player.Abilities();")
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/player/Player",
                "isCreative",
                "boolean",
                "()Z",
            ),
            "return horizonCreative;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/player/Player",
                "getScoreboardName",
                "java.lang.String",
                "()Ljava/lang/String;",
            ),
            "return horizonScoreboardName;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/player/Player",
                "getExperienceProgress",
                "float",
                "()F",
            ),
            "return horizonExperienceProgress;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/player/Player",
                "getInventory",
                "net.minecraft.world.entity.player.Inventory",
                "()Lnet/minecraft/world/entity/player/Inventory;",
            ),
            "return horizonInventory;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/player/Player",
                "getAbilities",
                "net.minecraft.world.entity.player.Abilities",
                "()Lnet/minecraft/world/entity/player/Abilities;",
            ),
            "return horizonAbilities;"
        );
    }

    #[test]
    fn player_inventory_and_abilities_are_bounded() {
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/player/Inventory",
                "getContainerSize",
                "int",
                "()I",
            ),
            "return horizonItems.length;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/player/Inventory",
                "getSelected",
                "net.minecraft.world.item.ItemStack",
                "()Lnet/minecraft/world/item/ItemStack;",
            ),
            "return getItem(horizonSelected);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/player/Inventory",
                "clearContent",
                "void",
                "()V",
            ),
            "for (int i = 0; i < horizonItems.length; i++) horizonItems[i] = new net.minecraft.world.item.ItemStack();"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/player/Abilities",
                "getFlyingSpeed",
                "float",
                "()F",
            ),
            "return horizonFlyingSpeed;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/player/Abilities",
                "setWalkingSpeed",
                "void",
                "(F)V",
            ),
            "horizonWalkingSpeed = p0;"
        );
        let inventory = Class {
            name: "net/minecraft/world/entity/player/Inventory".to_owned(),
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &inventory,
            Some(&BTreeSet::new()),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("horizonItems = new net.minecraft.world.item.ItemStack[36]"));
    }

    #[test]
    fn aabb_facade_preserves_bounds_and_geometry_queries() {
        let class = Class {
            name: "net/minecraft/world/phys/AABB".to_owned(),
            superclass: "java/lang/Object".to_owned(),
            declared: vec![
                Declared {
                    name: "minX".to_owned(),
                    descriptor: "D".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "minY".to_owned(),
                    descriptor: "D".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "minZ".to_owned(),
                    descriptor: "D".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "maxX".to_owned(),
                    descriptor: "D".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "maxY".to_owned(),
                    descriptor: "D".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "maxZ".to_owned(),
                    descriptor: "D".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "<init>".to_owned(),
                    descriptor: "(DDDDDD)V".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getCenter".to_owned(),
                    descriptor: "()Lnet/minecraft/world/phys/Vec3;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([(
                "getCenter".to_owned(),
                "()Lnet/minecraft/world/phys/Vec3;".to_owned(),
            )])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("protected double horizonMinX;"));
        assert!(source.contains("this.horizonMinX = p0;"));
        assert!(source.contains("public net.minecraft.world.phys.Vec3 getCenter()"));
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/phys/AABB",
                "getXsize",
                "double",
                "()D",
            ),
            "return horizonMaxX - horizonMinX;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/phys/AABB",
                "inflate",
                "net.minecraft.world.phys.AABB",
                "(D)Lnet/minecraft/world/phys/AABB;",
            ),
            "return new net.minecraft.world.phys.AABB(horizonMinX - p0, horizonMinY - p0, horizonMinZ - p0, horizonMaxX + p0, horizonMaxY + p0, horizonMaxZ + p0);"
        );
    }

    #[test]
    fn block_hit_result_facade_preserves_26_3_hit_state() {
        assert_eq!(
            runtime_body(
                "net/minecraft/world/phys/HitResult",
                "getLocation",
                "net.minecraft.world.phys.Vec3"
            ),
            "return horizonLocation;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/phys/HitResult",
                "getDirection",
                "net.minecraft.core.Direction"
            ),
            "return horizonDirection;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/phys/BlockHitResult",
                "getBlockPos",
                "net.minecraft.core.BlockPos",
                "()Lnet/minecraft/core/BlockPos;",
            ),
            "return horizonBlockPos;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/phys/BlockHitResult",
                "withDirection",
                "net.minecraft.world.phys.BlockHitResult",
                "(Lnet/minecraft/core/Direction;)Lnet/minecraft/world/phys/BlockHitResult;",
            ),
            "return new BlockHitResult(horizonLocation, p0, horizonBlockPos, horizonInside);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/phys/BlockHitResult",
                "withPosition",
                "net.minecraft.world.phys.BlockHitResult",
                "(Lnet/minecraft/core/BlockPos;)Lnet/minecraft/world/phys/BlockHitResult;",
            ),
            "return new BlockHitResult(horizonLocation, horizonDirection, p0, horizonInside);"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/phys/BlockHitResult",
                "miss",
                "net.minecraft.world.phys.BlockHitResult",
                "(Lnet/minecraft/world/phys/Vec3;Lnet/minecraft/core/Direction;Lnet/minecraft/core/BlockPos;)Lnet/minecraft/world/phys/BlockHitResult;",
            ),
            "return new BlockHitResult(p0, p1, p2, false);"
        );

        let class = Class {
            name: "net/minecraft/world/phys/BlockHitResult".to_owned(),
            superclass: "net/minecraft/world/phys/HitResult".to_owned(),
            declared: vec![
                Declared {
                    name: "<init>".to_owned(),
                    descriptor: "(Lnet/minecraft/world/phys/Vec3;Lnet/minecraft/core/Direction;Lnet/minecraft/core/BlockPos;Z)V".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "getBlockPos".to_owned(),
                    descriptor: "()Lnet/minecraft/core/BlockPos;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([(
                "getBlockPos".to_owned(),
                "()Lnet/minecraft/core/BlockPos;".to_owned(),
            )])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("horizonBlockPos = new net.minecraft.core.BlockPos()"));
        assert!(source.contains("this.horizonLocation = p0; this.horizonDirection = p1;"));
        assert!(source.contains("public net.minecraft.core.BlockPos getBlockPos()"));
    }

    #[test]
    fn accessor_mixin_methods_are_materialized_on_target_stub() {
        let class = Class {
            name: "net/minecraft/Test".to_owned(),
            ..Class::default()
        };
        let mut mixins = BTreeMap::new();
        mixins.insert(
            class.name.clone(),
            vec![MixinImpl {
                interface: "example/TestAccessor".to_owned(),
                methods: vec![Declared {
                    name: "getValue".to_owned(),
                    descriptor: "()I".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                }],
            }],
        );
        let mut report = Report::default();
        let source = render(
            &class,
            None,
            &BTreeMap::new(),
            &BTreeSet::from(["example/TestAccessor".to_owned()]),
            mixins.get(&class.name),
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("implements example.TestAccessor"));
        assert!(source.contains("public int getValue() { return 0; }"));
    }

    #[test]
    fn accessor_object_defaults_are_non_null_for_collections() {
        assert_eq!(
            default_value_for_java_type("java.util.List"),
            "java.util.Collections.emptyList()"
        );
        assert_eq!(
            default_value_for_java_type("java.util.Optional"),
            "java.util.Optional.empty()"
        );
        assert_eq!(default_value_for_java_type("int[]"), "new int[0]");
        assert_eq!(default_value_for_java_type("int[][]"), "null");
        assert_eq!(
            default_value_for_java_type("java.lang.Thread"),
            "java.lang.Thread.currentThread()"
        );
        assert!(safe_mixin_method(&Declared {
            name: "getThread".to_owned(),
            descriptor: "()Ljava/lang/Thread;".to_owned(),
            access: 0x0001,
            kind: MemberKind::Method,
        }));
        assert!(safe_mixin_method(&Declared {
            name: "getParts".to_owned(),
            descriptor: "()[Lnet/minecraft/client/model/ModelPart;".to_owned(),
            access: 0x0001,
            kind: MemberKind::Method,
        }));
    }

    #[test]
    fn interaction_result_facade_preserves_26_3_nested_value_shape() {
        let class = Class {
            name: "net/minecraft/world/InteractionResult".to_owned(),
            access: 0x0201,
            declared: vec![
                Declared {
                    name: "SUCCESS".to_owned(),
                    descriptor: "Lnet/minecraft/world/InteractionResult$Success;".to_owned(),
                    access: 0x0019,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "SUCCESS_SERVER".to_owned(),
                    descriptor: "Lnet/minecraft/world/InteractionResult$Success;".to_owned(),
                    access: 0x0019,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "CONSUME".to_owned(),
                    descriptor: "Lnet/minecraft/world/InteractionResult$Success;".to_owned(),
                    access: 0x0019,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "FAIL".to_owned(),
                    descriptor: "Lnet/minecraft/world/InteractionResult$Fail;".to_owned(),
                    access: 0x0019,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "PASS".to_owned(),
                    descriptor: "Lnet/minecraft/world/InteractionResult$Pass;".to_owned(),
                    access: 0x0019,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "TRY_WITH_EMPTY_HAND".to_owned(),
                    descriptor: "Lnet/minecraft/world/InteractionResult$TryEmptyHandInteraction;"
                        .to_owned(),
                    access: 0x0019,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "consumesAction".to_owned(),
                    descriptor: "()Z".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let mut report = Report::default();
        let source = render(
            &class,
            Some(&BTreeSet::from([(
                "consumesAction".to_owned(),
                "()Z".to_owned(),
            )])),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("public interface InteractionResult"));
        assert!(source.contains("public static final net.minecraft.world.InteractionResult.Success SUCCESS = new net.minecraft.world.InteractionResult.Success"));
        assert!(source.contains("InteractionResult.SwingSource.PREDICTED"));
        assert!(source.contains("consumesAction()"));
        assert!(source.contains("this instanceof InteractionResult.Success"));
        assert_eq!(
            runtime_body(
                "net/minecraft/world/InteractionResult$ItemContext",
                "wasItemInteraction",
                "boolean"
            ),
            "return wasItemInteraction;"
        );
    }

    #[test]
    fn entity_facade_retains_identity_and_environment_flags() {
        let source = include_str!("facade.rs");
        assert!(source.contains("protected net.minecraft.world.entity.EntityType horizonType"));
        assert!(source.contains("protected int horizonEntityId;"));
        assert!(source.contains("protected boolean horizonOnFire;"));
        assert!(source.contains("protected boolean horizonCrouching;"));
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "getId",
                "int",
                "()I",
            ),
            "return horizonEntityId;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "setId",
                "void",
                "(I)V",
            ),
            "horizonEntityId = p0;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "getStringUUID",
                "java.lang.String",
                "()Ljava/lang/String;",
            ),
            "return horizonUuid.toString();"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "setSecondsOnFire",
                "void",
                "(IZ)V",
            ),
            "horizonOnFire = p0 > 0;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "isShiftKeyDown",
                "boolean",
                "()Z",
            ),
            "return horizonCrouching;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/Entity",
                "setSprinting",
                "void",
                "(Z)V",
            ),
            "horizonSprinting = p0;"
        );
    }

    #[test]
    fn registry_facade_exposes_bounded_ordered_utilities() {
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/Registry",
                "getId",
                "int",
                "(Ljava/lang/Object;)I",
            ),
            "int id = 0; for (Object value : horizonEntries.values()) { if (java.util.Objects.equals(value, p0)) return id; id++; } return -1;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/core/DefaultedRegistry",
                "byId",
                "java.lang.Object",
                "(I)Ljava/lang/Object;",
            ),
            "int id = p0; if (id < 0 || id >= horizonEntries.size()) return null; return (java.lang.Object) horizonEntries.values().toArray()[id];"
        );
        assert_eq!(
            runtime_body("net/minecraft/core/Registry", "size", "int"),
            "return horizonEntries.size();"
        );
        assert_eq!(
            runtime_body("net/minecraft/core/Registry", "entrySet", "java.util.Set",),
            "return horizonEntries.entrySet();"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/core/Registry",
                "stream",
                "java.util.stream.Stream",
            ),
            "return horizonEntries.values().stream();"
        );
    }

    #[test]
    fn extracts_nested_jars_for_javac_and_cleans_them_up() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("horizon-facade-nested-{stamp}"));
        let outer = root.with_extension("jar");
        let nested_bytes = {
            let mut bytes = std::io::Cursor::new(Vec::new());
            {
                let mut jar = zip::ZipWriter::new(&mut bytes);
                jar.start_file("marker.txt", zip::write::SimpleFileOptions::default())
                    .unwrap();
                std::io::Write::write_all(&mut jar, b"nested").unwrap();
                jar.finish().unwrap();
            }
            bytes.into_inner()
        };
        {
            let file = fs::File::create(&outer).unwrap();
            let mut jar = zip::ZipWriter::new(file);
            jar.start_file(
                "META-INF/jars/inner.jar",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            std::io::Write::write_all(&mut jar, &nested_bytes).unwrap();
            jar.finish().unwrap();
        }
        let extracted = extract_nested_jars(std::slice::from_ref(&outer), &root).unwrap();
        assert_eq!(extracted.jars.len(), 1);
        assert!(extracted.jars[0].is_file());
        drop(extracted);
        assert!(!root.exists());
        let _ = fs::remove_file(outer);
    }

    #[test]
    fn client_facade_descriptors_match_the_api_shapes() {
        // HUD: functional holder with a static `EVENT`, invoked per frame.
        for fragment in [
            "@FunctionalInterface\npublic interface HudRenderCallback {",
            "void onHudRender({gui} drawContext, {ticks} tickCounter);",
            "Event<HudRenderCallback> EVENT = Event.horizon(horizonListeners ->",
            "for (HudRenderCallback horizonListener : horizonListeners) {",
            "horizonListener.onHudRender(drawContext, tickCounter);",
        ] {
            assert!(
                FABRIC_HUD_RENDER_CALLBACK.contains(fragment),
                "missing HUD fragment: {fragment}"
            );
        }
        // Tooltip: the four-argument signature, with the mutable line list last.
        for fragment in [
            "void getTooltip({item_stack} stack, {tooltip_context} tooltipContext, {tooltip_type} tooltipType, List<{component}> lines);",
            "Event<ItemTooltipCallback> EVENT = Event.horizon(horizonListeners ->",
            "for (ItemTooltipCallback horizonListener : horizonListeners) {",
            "horizonListener.getTooltip(stack, tooltipContext, tooltipType, lines);",
        ] {
            assert!(
                FABRIC_ITEM_TOOLTIP_CALLBACK.contains(fragment),
                "missing tooltip fragment: {fragment}"
            );
        }
        // Key binding: registration returns the mapping it was handed.
        for fragment in [
            "public static {key_mapping} registerKeyBinding({key_mapping} binding) {",
            "if (binding != null) HORIZON_BINDINGS.add(binding);",
            "return binding;",
            "public static java.util.List<{key_mapping}> horizonKeyBindings() {",
        ] {
            assert!(
                FABRIC_KEY_BINDING_HELPER.contains(fragment),
                "missing key-binding fragment: {fragment}"
            );
        }
        // Client ticks reuse the server event generator, so their shape is the
        // same ordered-dispatch holder.
        let ticks = fabric_event_source(
            "net.fabricmc.fabric.api.client.event.lifecycle.v1",
            "ClientTickEvents",
            "doc",
            FABRIC_CLIENT_TICK_EVENTS,
        );
        for (field, callback, method) in [
            ("START_CLIENT_TICK", "StartTick", "onStartTick"),
            ("END_CLIENT_TICK", "EndTick", "onEndTick"),
        ] {
            assert!(ticks.contains(&format!(
                "public static final Event<{callback}> {field} = Event.horizon(horizonListeners ->"
            )));
            assert!(ticks.contains(&format!(
                "void {method}(net.minecraft.client.Minecraft p0);"
            )));
            assert!(ticks.contains(&format!(
                "for ({callback} horizonListener : horizonListeners) horizonListener.{method}(p0);"
            )));
        }
        // NeoForge: registrations are retained; tick and render are markers.
        for fragment in [
            "public class RegisterKeyMappingsEvent extends Event {",
            "public void register({key_mapping} binding) {",
            "if (binding != null) horizonKeyMappings.add(binding);",
            "public java.util.List<{key_mapping}> horizonKeyMappings() {",
        ] {
            assert!(
                NEOFORGE_REGISTER_KEY_MAPPINGS.contains(fragment),
                "missing NeoForge fragment: {fragment}"
            );
        }
        assert!(
            NEOFORGE_CLIENT_TICK_EVENT
                .contains("public static final class Pre extends ClientTickEvent {}")
        );
        assert!(
            NEOFORGE_CLIENT_TICK_EVENT
                .contains("public static final class Post extends ClientTickEvent {}")
        );
        assert!(
            NEOFORGE_RENDER_GUI_EVENT
                .contains("public {gui} getGuiGraphics() { return horizonGuiGraphics; }")
        );
    }

    #[test]
    fn client_types_resolve_against_the_game_jar() {
        let game = |names: &[&str]| -> BTreeMap<String, Class> {
            names
                .iter()
                .map(|name| {
                    (
                        (*name).to_owned(),
                        Class {
                            name: (*name).to_owned(),
                            ..Class::default()
                        },
                    )
                })
                .collect()
        };
        let none = BTreeSet::new();
        assert!(resolve_client_types(&game(&[]), &none, &none).is_empty());
        // 26.3's renamed class wins over the names it replaced.
        let renamed = game(&[
            "net/minecraft/client/gui/GuiGraphicsExtractor",
            "net/minecraft/client/gui/GuiGraphics",
            "net/minecraft/client/gui/DrawContext",
        ]);
        assert_eq!(
            resolve_client_types(&renamed, &none, &none)["gui"],
            "net/minecraft/client/gui/GuiGraphicsExtractor"
        );
        // The type the mod itself names wins over the table order, so an
        // explicitly typed listener overrides the signature it was built for.
        let referenced = BTreeSet::from(["net/minecraft/client/gui/DrawContext".to_owned()]);
        assert_eq!(
            resolve_client_types(&renamed, &none, &referenced)["gui"],
            "net/minecraft/client/gui/DrawContext"
        );
        // A library jar's class is real and never becomes a façade role.
        let library = BTreeSet::from(["net/minecraft/client/gui/GuiGraphicsExtractor".to_owned()]);
        assert_eq!(
            resolve_client_types(&renamed, &library, &none)["gui"],
            "net/minecraft/client/gui/GuiGraphics"
        );
        // Nested roles are written the Java way.
        let nested = game(&["net/minecraft/world/item/Item$TooltipContext"]);
        let resolved = resolve_client_types(&nested, &none, &none);
        assert_eq!(
            fill_types("import {tooltip_context};", &resolved),
            "import net.minecraft.world.item.Item.TooltipContext;"
        );
        // A `$` name the jar lacks leaves the role unresolved.
        assert!(
            !resolve_client_types(&game(&["net/minecraft/item/ItemStack"]), &none, &none)
                .contains_key("tooltip_context")
        );
    }

    #[test]
    fn client_shims_only_fill_an_absent_api_class() {
        let source = include_str!("facade.rs");
        assert!(source.contains("let uses_fabric_client = target"));
        assert!(source.contains("let uses_neoforge_client = uses_neoforge"));
        assert!(source.contains("resolve_client_types(&game, &lib_names, &wanted)"));
        // Absent API classes only, and no fabricated client type.
        assert!(source.contains("if self.available.contains(class) {"));
        assert!(source.contains("(needs client type {})"));
        // `Event` is written once even when the server and client slices both
        // need it.
        assert!(source.contains("if !uses_fabric_events {"));
        for class in [
            "net/fabricmc/fabric/api/client/rendering/v1/HudRenderCallback",
            "net/fabricmc/fabric/api/client/event/lifecycle/v1/ClientTickEvents",
            "net/fabricmc/fabric/api/client/keybinding/v1/KeyBindingHelper",
            "net/fabricmc/fabric/api/client/item/tooltip/v1/ItemTooltipCallback",
            "net/neoforged/neoforge/client/event/RegisterKeyMappingsEvent",
            "net/neoforged/neoforge/client/event/ClientTickEvent",
            "net/neoforged/neoforge/client/event/RenderGuiEvent",
        ] {
            assert!(
                source.contains(&format!("\"{class}\"")),
                "client shim not registered: {class}"
            );
        }
        for role in [
            "gui",
            "ticks",
            "minecraft",
            "key_mapping",
            "item_stack",
            "tooltip_context",
            "tooltip_type",
            "component",
        ] {
            assert!(
                CLIENT_TYPE_CANDIDATES.iter().any(|(name, _)| *name == role),
                "missing client role: {role}"
            );
        }
        // Every client type is a placeholder: a hard-coded name would freeze a
        // 26.3 rename into the template.
        for template in [
            FABRIC_HUD_RENDER_CALLBACK,
            FABRIC_ITEM_TOOLTIP_CALLBACK,
            FABRIC_KEY_BINDING_HELPER,
            NEOFORGE_REGISTER_KEY_MAPPINGS,
            NEOFORGE_RENDER_GUI_EVENT,
        ] {
            assert!(
                !template.contains("net.minecraft."),
                "hard-coded client type in a template"
            );
        }
    }

    /// The client HUD, tooltip and key-binding shims are compiled and run for
    /// real: ordered dispatch and retained registrations are behaviour, not a
    /// shape. Skipped cleanly when no JDK is installed.
    #[test]
    fn client_facade_compiles_and_dispatches_in_order() {
        use std::{path::PathBuf, process::Command};
        let types: BTreeMap<&'static str, String> = BTreeMap::from([
            (
                "gui",
                "net/minecraft/client/gui/GuiGraphicsExtractor".to_owned(),
            ),
            (
                "ticks",
                "net/minecraft/client/render/RenderTickCounter".to_owned(),
            ),
            ("minecraft", "net/minecraft/client/Minecraft".to_owned()),
            ("key_mapping", "net/minecraft/client/KeyMapping".to_owned()),
            (
                "item_stack",
                "net/minecraft/world/item/ItemStack".to_owned(),
            ),
            (
                "tooltip_context",
                "net/minecraft/world/item/Item$TooltipContext".to_owned(),
            ),
            (
                "tooltip_type",
                "net/minecraft/world/item/TooltipFlag".to_owned(),
            ),
            (
                "component",
                "net/minecraft/network/chat/Component".to_owned(),
            ),
        ]);
        let probe = r#"public class ClientProbe {
    public static void main(String[] args) {
        java.util.List<String> seen = new java.util.ArrayList<>();
        net.fabricmc.fabric.api.client.rendering.v1.HudRenderCallback.EVENT.register((context, ticks) -> seen.add("first"));
        net.fabricmc.fabric.api.client.rendering.v1.HudRenderCallback.EVENT.register((context, ticks) -> seen.add("second"));
        net.fabricmc.fabric.api.client.rendering.v1.HudRenderCallback.EVENT.invoker().onHudRender(null, null);
        if (!seen.equals(java.util.Arrays.asList("first", "second"))) throw new AssertionError("hud order " + seen);

        java.util.List<net.minecraft.network.chat.Component> lines = new java.util.ArrayList<>();
        net.minecraft.network.chat.Component first = new net.minecraft.network.chat.Component();
        first.horizonName = "first";
        net.minecraft.network.chat.Component second = new net.minecraft.network.chat.Component();
        second.horizonName = "second";
        net.fabricmc.fabric.api.client.item.tooltip.v1.ItemTooltipCallback.EVENT.register((stack, context, flag, target) -> target.add(first));
        net.fabricmc.fabric.api.client.item.tooltip.v1.ItemTooltipCallback.EVENT.register((stack, context, flag, target) -> target.add(second));
        net.fabricmc.fabric.api.client.item.tooltip.v1.ItemTooltipCallback.EVENT.invoker().getTooltip(null, null, null, lines);
        if (lines.size() != 2 || !"first".equals(lines.get(0).horizonName) || !"second".equals(lines.get(1).horizonName)) throw new AssertionError("tooltip order " + lines.size());

        net.fabricmc.fabric.api.client.event.lifecycle.v1.ClientTickEvents.START_CLIENT_TICK.register(client -> seen.add("tick"));
        net.fabricmc.fabric.api.client.event.lifecycle.v1.ClientTickEvents.START_CLIENT_TICK.invoker().onStartTick(null);
        if (!"tick".equals(seen.get(seen.size() - 1))) throw new AssertionError("tick " + seen);

        net.minecraft.client.KeyMapping one = new net.minecraft.client.KeyMapping();
        net.minecraft.client.KeyMapping two = new net.minecraft.client.KeyMapping();
        if (net.fabricmc.fabric.api.client.keybinding.v1.KeyBindingHelper.registerKeyBinding(one) != one) throw new AssertionError("key binding identity");
        net.fabricmc.fabric.api.client.keybinding.v1.KeyBindingHelper.registerKeyBinding(two);
        net.fabricmc.fabric.api.client.keybinding.v1.KeyBindingHelper.registerKeyBinding(null);
        if (net.fabricmc.fabric.api.client.keybinding.v1.KeyBindingHelper.horizonKeyBindings().size() != 2) throw new AssertionError("key bindings");

        net.neoforged.neoforge.client.event.RegisterKeyMappingsEvent event = new net.neoforged.neoforge.client.event.RegisterKeyMappingsEvent();
        event.register(one);
        event.register(two);
        event.register(null);
        if (event.horizonKeyMappings().size() != 2) throw new AssertionError("mappings " + event.horizonKeyMappings().size());
        if (event.horizonKeyMappings().get(0) != one || event.horizonKeyMappings().get(1) != two) throw new AssertionError("mapping order");
        if (!(new net.neoforged.neoforge.client.event.ClientTickEvent.Pre() instanceof net.neoforged.bus.api.Event)) throw new AssertionError("tick marker");
        net.neoforged.neoforge.client.event.RenderGuiEvent.Pre render = new net.neoforged.neoforge.client.event.RenderGuiEvent.Pre(null);
        if (render.getGuiGraphics() != null) throw new AssertionError("render gui");
        System.out.println("horizon client facade ok");
    }
}
"#;
        let root =
            std::env::temp_dir().join(format!("horizon-facade-client-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        let mut files = Vec::new();
        let mut write = |relative: &str, text: &str| {
            let file = src.join(relative);
            fs::create_dir_all(file.parent().expect("package directory")).expect("create package");
            fs::write(&file, text).expect("write java shim");
            files.push(file);
        };
        write("net/fabricmc/fabric/api/event/Event.java", FABRIC_EVENT);
        write(
            "net/fabricmc/fabric/api/client/rendering/v1/HudRenderCallback.java",
            &fill_types(FABRIC_HUD_RENDER_CALLBACK, &types),
        );
        write(
            "net/fabricmc/fabric/api/client/item/tooltip/v1/ItemTooltipCallback.java",
            &fill_types(FABRIC_ITEM_TOOLTIP_CALLBACK, &types),
        );
        write(
            "net/fabricmc/fabric/api/client/keybinding/v1/KeyBindingHelper.java",
            &fill_types(FABRIC_KEY_BINDING_HELPER, &types),
        );
        write(
            "net/fabricmc/fabric/api/client/event/lifecycle/v1/ClientTickEvents.java",
            &fabric_event_source(
                "net.fabricmc.fabric.api.client.event.lifecycle.v1",
                "ClientTickEvents",
                "doc",
                FABRIC_CLIENT_TICK_EVENTS,
            ),
        );
        // The smallest game types the callback signatures name.
        write(
            "net/minecraft/client/gui/GuiGraphicsExtractor.java",
            "package net.minecraft.client.gui;\npublic class GuiGraphicsExtractor {}\n",
        );
        write(
            "net/minecraft/client/render/RenderTickCounter.java",
            "package net.minecraft.client.render;\npublic class RenderTickCounter {}\n",
        );
        write(
            "net/minecraft/client/Minecraft.java",
            "package net.minecraft.client;\npublic class Minecraft {}\n",
        );
        write(
            "net/minecraft/client/KeyMapping.java",
            "package net.minecraft.client;\npublic class KeyMapping {}\n",
        );
        write(
            "net/minecraft/world/item/ItemStack.java",
            "package net.minecraft.world.item;\npublic class ItemStack {}\n",
        );
        write(
            "net/minecraft/world/item/TooltipFlag.java",
            "package net.minecraft.world.item;\npublic class TooltipFlag {}\n",
        );
        write(
            "net/minecraft/network/chat/Component.java",
            "package net.minecraft.network.chat;\npublic class Component { public String horizonName; }\n",
        );
        write(
            "net/minecraft/world/item/Item.java",
            "package net.minecraft.world.item;\npublic class Item { public static class TooltipContext {} }\n",
        );
        write("net/neoforged/bus/api/Event.java", NEOFORGE_EVENT);
        for (relative, source) in [
            (
                "net/neoforged/neoforge/client/event/RegisterKeyMappingsEvent.java",
                fill_types(NEOFORGE_REGISTER_KEY_MAPPINGS, &types),
            ),
            (
                "net/neoforged/neoforge/client/event/ClientTickEvent.java",
                NEOFORGE_CLIENT_TICK_EVENT.to_owned(),
            ),
            (
                "net/neoforged/neoforge/client/event/RenderGuiEvent.java",
                fill_types(NEOFORGE_RENDER_GUI_EVENT, &types),
            ),
        ] {
            write(relative, &source);
        }
        write("ClientProbe.java", probe);
        fs::create_dir_all(&classes).expect("create classes directory");
        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let javac = jdk_bin
            .as_ref()
            .map(|bin| bin.join("javac"))
            .unwrap_or_else(|| PathBuf::from("javac"));
        let Ok(javac_status) = Command::new(&javac)
            .arg("-nowarn")
            .arg("-d")
            .arg(&classes)
            .args(&files)
            .status()
        else {
            let _ = fs::remove_dir_all(&root);
            return;
        };
        assert!(
            javac_status.success(),
            "javac rejected the generated client façade"
        );
        let java = jdk_bin
            .as_ref()
            .map(|bin| bin.join("java"))
            .unwrap_or_else(|| PathBuf::from("java"));
        let output = Command::new(&java)
            .arg("-cp")
            .arg(&classes)
            .arg("ClientProbe")
            .output()
            .expect("run the client probe");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "client probe failed: {stderr}");
        assert!(
            stdout.contains("horizon client facade ok"),
            "probe output: {stdout}{stderr}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn screen_facade_shapes_are_guarded_and_resolved() {
        let types = BTreeMap::from([
            ("minecraft", "net/minecraft/client/Minecraft".to_owned()),
            (
                "screen",
                "net/minecraft/client/gui/screens/Screen".to_owned(),
            ),
            (
                "gui",
                "net/minecraft/client/gui/GuiGraphicsExtractor".to_owned(),
            ),
            (
                "clickable_widget",
                "net/minecraft/client/gui/components/ClickableWidget".to_owned(),
            ),
        ]);
        // The widget lookups are markers: no widget tree exists behind them.
        for fragment in [
            "public static List<{clickable_widget}> getButtons({screen} screen) {",
            "return Collections.emptyList();",
            "public static <T extends {clickable_widget}> T getWidget({screen} screen, Class<T> widgetClass) {",
            "public static <T extends {clickable_widget}> T getFocused({screen} screen, Class<T> widgetClass) {",
            "return null;",
        ] {
            assert!(
                FABRIC_SCREENS_SOURCE.contains(fragment),
                "missing Screens fragment: {fragment}"
            );
        }
        // NeoForge's markers: `Init`/`Render` phases with their `Pre`/`Post`
        // nested classes and the bounded screen accessor.
        for fragment in [
            "public abstract class ScreenEvent extends Event {",
            "public {screen} getScreen() { return horizonScreen; }",
            "public static final class Post extends Init {",
            "public static class Pre extends Render {",
            "public static final class Post extends Render {",
        ] {
            assert!(
                NEOFORGE_SCREEN_EVENT_SOURCE.contains(fragment),
                "missing ScreenEvent fragment: {fragment}"
            );
        }
        // Every game type is a role placeholder, never a frozen 26.3 name.
        for template in [
            FABRIC_SCREEN_EVENTS_SOURCE,
            FABRIC_SCREENS_SOURCE,
            NEOFORGE_SCREEN_EVENT_SOURCE,
        ] {
            assert!(
                !template.contains("net.minecraft."),
                "hard-coded client type in a screen template"
            );
        }

        // The roles resolve from the game jar; a server-shaped jar resolves
        // none of them.
        let client_game: BTreeMap<String, Class> = [
            "net/minecraft/client/Minecraft",
            "net/minecraft/client/gui/GuiGraphicsExtractor",
            "net/minecraft/client/gui/screens/Screen",
            "net/minecraft/client/gui/screens/inventory/AbstractContainerScreen",
            "net/minecraft/client/gui/components/ClickableWidget",
            "net/minecraft/network/chat/Component",
        ]
        .iter()
        .map(|name| {
            (
                (*name).to_owned(),
                Class {
                    name: (*name).to_owned(),
                    ..Class::default()
                },
            )
        })
        .collect();
        let resolved = resolve_client_types(&client_game, &BTreeSet::new(), &BTreeSet::new());
        for role in [
            "screen",
            "minecraft",
            "gui",
            "abstract_container_screen",
            "clickable_widget",
            "component",
        ] {
            assert!(
                resolved.contains_key(role),
                "screen role did not resolve: {role}"
            );
        }
        assert!(
            resolve_client_types(&BTreeMap::new(), &BTreeSet::new(), &BTreeSet::new()).is_empty()
        );

        // Emission: a real API class is never shadowed, a missing role is
        // reported, and an emitted shim lands at the API's path.
        let root =
            std::env::temp_dir().join(format!("horizon-facade-screen-shim-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create shim directory");
        let mut files = Vec::new();
        let mut report = Report::default();
        let available = BTreeSet::from([FABRIC_SCREEN_EVENTS.to_owned()]);
        let shims = ClientShims {
            src: &root,
            available: &available,
            types: &types,
        };
        assert!(
            !shims
                .emit(
                    &mut files,
                    &mut report,
                    FABRIC_SCREEN_EVENTS,
                    &["minecraft", "screen", "gui"],
                    || "shadowed".to_owned(),
                )
                .expect("guard the real API class")
        );
        assert!(files.is_empty() && report.missing.is_empty());
        let empty = BTreeMap::new();
        let shims = ClientShims {
            src: &root,
            available: &BTreeSet::new(),
            types: &empty,
        };
        assert!(
            !shims
                .emit(
                    &mut files,
                    &mut report,
                    FABRIC_SCREEN_EVENTS,
                    &["minecraft", "screen", "gui"],
                    || "invented".to_owned(),
                )
                .expect("report a missing role")
        );
        assert_eq!(
            report.missing,
            vec![
                "net/fabricmc/fabric/api/client/screen/v1/ScreenEvents \
                 (needs client type minecraft, screen, gui)"
            ]
        );
        let shims = ClientShims {
            src: &root,
            available: &BTreeSet::new(),
            types: &types,
        };
        for (class, requires, source) in [
            (
                FABRIC_SCREENS,
                &["screen", "clickable_widget"][..],
                FABRIC_SCREENS_SOURCE,
            ),
            (
                NEOFORGE_SCREEN_EVENT,
                &["screen"][..],
                NEOFORGE_SCREEN_EVENT_SOURCE,
            ),
        ] {
            assert!(
                shims
                    .emit(&mut files, &mut report, class, requires, || fill_types(
                        source, &types
                    ))
                    .expect("emit the screen shim")
            );
        }
        assert_eq!(files.len(), 2);
        assert!(files[0].ends_with("net/fabricmc/fabric/api/client/screen/v1/Screens.java"));
        assert!(files[1].ends_with("net/neoforged/neoforge/client/event/ScreenEvent.java"));
        let _ = fs::remove_dir_all(&root);
    }

    /// The screen hooks and markers are compiled and run for real: ordered
    /// dispatch, the boolean veto, and the widget lookups. Skipped cleanly when
    /// no JDK is installed.
    #[test]
    fn fabric_screen_events_compile_and_dispatch_in_order() {
        use std::{path::PathBuf, process::Command};
        let types: BTreeMap<&'static str, String> = BTreeMap::from([
            ("minecraft", "net/minecraft/client/Minecraft".to_owned()),
            (
                "screen",
                "net/minecraft/client/gui/screens/Screen".to_owned(),
            ),
            (
                "gui",
                "net/minecraft/client/gui/GuiGraphicsExtractor".to_owned(),
            ),
            (
                "clickable_widget",
                "net/minecraft/client/gui/components/ClickableWidget".to_owned(),
            ),
        ]);
        let probe = r#"public class ScreenProbe {
    public static void main(String[] args) {
        java.util.List<String> seen = new java.util.ArrayList<>();
        net.fabricmc.fabric.api.client.screen.v1.ScreenEvents.AFTER_INIT.register((client, screen, scaledWidth, scaledHeight) -> seen.add("first:" + scaledWidth + "x" + scaledHeight));
        net.fabricmc.fabric.api.client.screen.v1.ScreenEvents.AFTER_INIT.register((client, screen, scaledWidth, scaledHeight) -> seen.add("second:" + scaledWidth + "x" + scaledHeight));
        net.fabricmc.fabric.api.client.screen.v1.ScreenEvents.AFTER_INIT.invoker().afterInit(null, null, 320, 240);
        if (!seen.equals(java.util.Arrays.asList("first:320x240", "second:320x240"))) throw new AssertionError("after init order " + seen);

        seen.clear();
        net.fabricmc.fabric.api.client.screen.v1.ScreenEvents.BEFORE_RENDER.register((screen, context, mouseX, mouseY, tickDelta) -> seen.add("before"));
        net.fabricmc.fabric.api.client.screen.v1.ScreenEvents.AFTER_RENDER.register((screen, context, mouseX, mouseY, tickDelta) -> seen.add("after"));
        net.fabricmc.fabric.api.client.screen.v1.ScreenEvents.BEFORE_RENDER.invoker().beforeRender(null, null, 1, 2, 0.5f);
        net.fabricmc.fabric.api.client.screen.v1.ScreenEvents.AFTER_RENDER.invoker().afterRender(null, null, 1, 2, 0.5f);
        if (!seen.equals(java.util.Arrays.asList("before", "after"))) throw new AssertionError("render order " + seen);

        if (!net.fabricmc.fabric.api.client.screen.v1.ScreenEvents.ALLOW_MOUSE_CLICK.invoker().allowMouseClick(null, 1, 2, 0)) throw new AssertionError("mouse click default");
        net.fabricmc.fabric.api.client.screen.v1.ScreenEvents.ALLOW_MOUSE_CLICK.register((screen, mouseX, mouseY, button) -> button != 1);
        if (!net.fabricmc.fabric.api.client.screen.v1.ScreenEvents.ALLOW_MOUSE_CLICK.invoker().allowMouseClick(null, 1, 2, 0)) throw new AssertionError("mouse click allow");
        if (net.fabricmc.fabric.api.client.screen.v1.ScreenEvents.ALLOW_MOUSE_CLICK.invoker().allowMouseClick(null, 1, 2, 1)) throw new AssertionError("mouse click veto");
        if (!net.fabricmc.fabric.api.client.screen.v1.ScreenEvents.ALLOW_KEY_PRESS.invoker().allowKeyPress(null, 65, 0, 0)) throw new AssertionError("key press default");

        if (net.fabricmc.fabric.api.client.screen.v1.Screens.getWidget(null, net.minecraft.client.gui.components.ClickableWidget.class) != null) throw new AssertionError("get widget marker");
        if (net.fabricmc.fabric.api.client.screen.v1.Screens.getFocused(null, net.minecraft.client.gui.components.ClickableWidget.class) != null) throw new AssertionError("get focused marker");
        if (!net.fabricmc.fabric.api.client.screen.v1.Screens.getButtons(null).isEmpty()) throw new AssertionError("get buttons marker");
        System.out.println("horizon screen facade ok");
    }
}
"#;
        let root =
            std::env::temp_dir().join(format!("horizon-facade-screen-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        let mut files = Vec::new();
        let mut write = |relative: &str, text: &str| {
            let file = src.join(relative);
            fs::create_dir_all(file.parent().expect("package directory")).expect("create package");
            fs::write(&file, text).expect("write java shim");
            files.push(file);
        };
        write("net/fabricmc/fabric/api/event/Event.java", FABRIC_EVENT);
        write(
            "net/fabricmc/fabric/api/client/screen/v1/ScreenEvents.java",
            &fill_types(FABRIC_SCREEN_EVENTS_SOURCE, &types),
        );
        write(
            "net/fabricmc/fabric/api/client/screen/v1/Screens.java",
            &fill_types(FABRIC_SCREENS_SOURCE, &types),
        );
        // The smallest game types the signatures name.
        write(
            "net/minecraft/client/Minecraft.java",
            "package net.minecraft.client;\npublic class Minecraft {}\n",
        );
        write(
            "net/minecraft/client/gui/GuiGraphicsExtractor.java",
            "package net.minecraft.client.gui;\npublic class GuiGraphicsExtractor {}\n",
        );
        write(
            "net/minecraft/client/gui/screens/Screen.java",
            "package net.minecraft.client.gui.screens;\npublic class Screen {}\n",
        );
        write(
            "net/minecraft/client/gui/components/ClickableWidget.java",
            "package net.minecraft.client.gui.components;\npublic class ClickableWidget {}\n",
        );
        write("ScreenProbe.java", probe);
        fs::create_dir_all(&classes).expect("create classes directory");
        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let javac = jdk_bin
            .as_ref()
            .map(|bin| bin.join("javac"))
            .unwrap_or_else(|| PathBuf::from("javac"));
        let Ok(javac_status) = Command::new(&javac)
            .arg("-nowarn")
            .arg("-d")
            .arg(&classes)
            .args(&files)
            .status()
        else {
            let _ = fs::remove_dir_all(&root);
            return;
        };
        assert!(
            javac_status.success(),
            "javac rejected the generated screen façade"
        );
        let java = jdk_bin
            .as_ref()
            .map(|bin| bin.join("java"))
            .unwrap_or_else(|| PathBuf::from("java"));
        let output = Command::new(&java)
            .arg("-cp")
            .arg(&classes)
            .arg("ScreenProbe")
            .output()
            .expect("run the screen probe");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "screen probe failed: {stderr}");
        assert!(
            stdout.contains("horizon screen facade ok"),
            "probe output: {stdout}{stderr}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn registration_facade_shapes_are_bounded_and_guarded() {
        // Every registration template is only its API shape: game types are
        // `{role}` placeholders, never a frozen 26.3 name.
        for (template, fragments) in [
            (
                FABRIC_ITEM_GROUP_EVENTS,
                &[
                    "public static Event<ModifyEntries> modifyEntriesEvent({resource_key} tab) {",
                    "void modifyEntries(FabricItemGroupEntries entries, {feature_flag_set} enabledFeatures);",
                ][..],
            ),
            (
                FABRIC_ITEM_GROUP_ENTRIES,
                &[
                    "public void add({item_stack} stack) {",
                    "public void accept({item_stack} stack) {",
                    "public void addAfter({item_stack} anchor, {item_stack}... stacks) {",
                    "public void addBefore({item_stack} anchor, {item_stack}... stacks) {",
                    "public List<{item_stack}> getDisplayStacks() {",
                ][..],
            ),
            (
                FABRIC_ENTITY_RENDERER_REGISTRY,
                &[
                    "public static void register({entity_type} type, {entity_renderer_provider} provider) {",
                ][..],
            ),
            (
                FABRIC_BLOCK_ENTITY_RENDERER_REGISTRY,
                &[
                    "public static void register({block_entity_type} type, {block_entity_renderer_provider} provider) {",
                ][..],
            ),
            (
                FABRIC_MODEL_LAYER_REGISTRY,
                &["{textured_model_data} createTexturedModelData();"][..],
            ),
            (
                FABRIC_COLOR_PROVIDER_REGISTRY,
                &[
                    "public ColorProvider register({block} target, ColorProvider provider) {",
                    "public ColorProvider register({item} target, ColorProvider provider) {",
                    "int getColor({block_state} state, {block_and_tint_getter} world, {block_pos} pos, int tintIndex);",
                ][..],
            ),
            (
                NEOFORGE_BUILD_CREATIVE_MODE_TAB_CONTENTS,
                &["public {creative_mode_tab} getTab() { return horizonTab; }"][..],
            ),
            (
                NEOFORGE_ENTITY_RENDERERS,
                &[
                    "public void registerEntityRenderer({entity_type} type, {entity_renderer_provider} provider) {",
                ][..],
            ),
        ] {
            for fragment in fragments {
                assert!(
                    template.contains(fragment),
                    "missing façade fragment: {fragment}"
                );
            }
            assert!(
                !template.contains("net.minecraft."),
                "hard-coded game type in a registration template"
            );
        }
        // A mod passes `MyRenderer::new` to both registries, so the provider
        // interfaces stay lambda targets in the generated game façade.
        assert!(functional_interface(
            "net/minecraft/client/renderer/entity/EntityRendererProvider"
        ));
        assert!(functional_interface(
            "net/minecraft/client/renderer/blockentity/BlockEntityRendererProvider"
        ));
        // Emission is gated by the API class and by role resolution.
        let source = include_str!("facade.rs");
        for marker in [
            "let uses_fabric_item_groups = target",
            "let uses_fabric_renderers = target",
            "let uses_neoforge_registration = uses_neoforge",
            "needed_roles.extend_from_slice",
        ] {
            assert!(source.contains(marker), "missing detection: {marker}");
        }
        for class in [
            "net/fabricmc/fabric/api/itemgroup/v1/ItemGroupEvents",
            "net/fabricmc/fabric/api/itemgroup/v1/FabricItemGroupEntries",
            "net/fabricmc/fabric/api/client/rendering/v1/EntityRendererRegistry",
            "net/fabricmc/fabric/api/client/rendering/v1/BlockEntityRendererRegistry",
            "net/fabricmc/fabric/api/client/rendering/v1/ModelLayerRegistry",
            "net/fabricmc/fabric/api/client/rendering/v1/ColorProviderRegistry",
            "net/neoforged/neoforge/event/BuildCreativeModeTabContentsEvent",
            "net/neoforged/neoforge/client/event/EntityRenderersEvent",
        ] {
            assert!(
                source.contains(&format!("\"{class}\"")),
                "registration shim not registered: {class}"
            );
        }
        for role in [
            "creative_mode_tab",
            "feature_flag_set",
            "resource_key",
            "entity_type",
            "block_entity_type",
            "entity_renderer_provider",
            "block_entity_renderer_provider",
            "model_layer_location",
            "textured_model_data",
        ] {
            assert!(
                CLIENT_TYPE_CANDIDATES.iter().any(|(name, _)| *name == role),
                "missing game role: {role}"
            );
        }
        // 26.3's `LayerDefinition` comes first; the name it replaced still
        // resolves for a jar compiled against it.
        let textured = CLIENT_TYPE_CANDIDATES
            .iter()
            .find(|(role, _)| *role == "textured_model_data")
            .map(|(_, names)| *names)
            .expect("textured model data role");
        assert_eq!(
            textured[0],
            "net/minecraft/client/model/geom/builders/LayerDefinition"
        );
        assert!(textured.contains(&"net/minecraft/client/model/geom/builders/TexturedModelData"));
    }

    #[test]
    fn item_group_facade_compiles_and_keeps_order() {
        use std::{path::PathBuf, process::Command};
        let types: BTreeMap<&'static str, String> = BTreeMap::from([
            (
                "creative_mode_tab",
                "net/minecraft/world/item/CreativeModeTab".to_owned(),
            ),
            (
                "feature_flag_set",
                "net/minecraft/world/flag/FeatureFlagSet".to_owned(),
            ),
            (
                "resource_key",
                "net/minecraft/resources/ResourceKey".to_owned(),
            ),
            (
                "item_stack",
                "net/minecraft/world/item/ItemStack".to_owned(),
            ),
        ]);
        let probe = r#"public class ItemGroupProbe {
    private static String names(java.util.List<net.minecraft.world.item.ItemStack> stacks) {
        StringBuilder out = new StringBuilder();
        for (net.minecraft.world.item.ItemStack stack : stacks) {
            if (out.length() > 0) out.append(',');
            out.append(stack.name);
        }
        return out.toString();
    }

    public static void main(String[] args) {
        net.minecraft.resources.ResourceKey tab = new net.minecraft.resources.ResourceKey();
        net.fabricmc.fabric.api.event.Event<net.fabricmc.fabric.api.itemgroup.v1.ItemGroupEvents.ModifyEntries> event =
            net.fabricmc.fabric.api.itemgroup.v1.ItemGroupEvents.modifyEntriesEvent(tab);
        if (net.fabricmc.fabric.api.itemgroup.v1.ItemGroupEvents.modifyEntriesEvent(tab) != event) throw new AssertionError("one event per tab");
        if (net.fabricmc.fabric.api.itemgroup.v1.ItemGroupEvents.horizonTabs().size() != 1) throw new AssertionError("tabs");

        net.minecraft.world.item.ItemStack a = new net.minecraft.world.item.ItemStack("a");
        net.minecraft.world.item.ItemStack b = new net.minecraft.world.item.ItemStack("b");
        net.minecraft.world.item.ItemStack c = new net.minecraft.world.item.ItemStack("c");
        event.register((entries, flags) -> entries.add(a));
        event.register((entries, flags) -> { entries.add(c); entries.addAfter(c, b); });
        net.fabricmc.fabric.api.itemgroup.v1.FabricItemGroupEntries entries =
            new net.fabricmc.fabric.api.itemgroup.v1.FabricItemGroupEntries();
        event.invoker().modifyEntries(entries, net.minecraft.world.flag.FeatureFlagSet.of());
        if (!names(entries.getDisplayStacks()).equals("a,c,b")) throw new AssertionError("dispatch order " + names(entries.getDisplayStacks()));

        entries.addBefore(b, a);
        if (!names(entries.getDisplayStacks()).equals("a,c,a,b")) throw new AssertionError("addBefore " + names(entries.getDisplayStacks()));
        entries.addAfter(new net.minecraft.world.item.ItemStack("missing"), c);
        if (!names(entries.getDisplayStacks()).equals("a,c,a,b,c")) throw new AssertionError("missing anchor " + names(entries.getDisplayStacks()));
        entries.accept(b);
        entries.add(null);
        if (!names(entries.getDisplayStacks()).equals("a,c,a,b,c,b")) throw new AssertionError("accept " + names(entries.getDisplayStacks()));
        if (!names(entries.getSearchTabStacks()).equals("a,c,a,b,c,b")) throw new AssertionError("search stacks");
        try {
            entries.getDisplayStacks().clear();
            throw new AssertionError("display stacks are mutable");
        } catch (UnsupportedOperationException expected) {}
        System.out.println("horizon item group facade ok");
    }
}
"#;
        let root =
            std::env::temp_dir().join(format!("horizon-facade-itemgroup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        let mut files = Vec::new();
        let mut write = |relative: &str, text: &str| {
            let file = src.join(relative);
            fs::create_dir_all(file.parent().expect("package directory")).expect("create package");
            fs::write(&file, text).expect("write java shim");
            files.push(file);
        };
        write("net/fabricmc/fabric/api/event/Event.java", FABRIC_EVENT);
        for (relative, template) in [
            (
                "net/fabricmc/fabric/api/itemgroup/v1/ItemGroupEvents.java",
                FABRIC_ITEM_GROUP_EVENTS,
            ),
            (
                "net/fabricmc/fabric/api/itemgroup/v1/FabricItemGroupEntries.java",
                FABRIC_ITEM_GROUP_ENTRIES,
            ),
        ] {
            write(relative, &fill_types(template, &types));
        }
        // The smallest game types the registration signatures name.
        for (relative, source) in [
            (
                "net/minecraft/world/item/ItemStack.java",
                "package net.minecraft.world.item;\npublic class ItemStack { public String name = \"\"; public ItemStack() {} public ItemStack(String name) { this.name = name; } }\n",
            ),
            (
                "net/minecraft/world/item/CreativeModeTab.java",
                "package net.minecraft.world.item;\npublic class CreativeModeTab {}\n",
            ),
            (
                "net/minecraft/world/flag/FeatureFlagSet.java",
                "package net.minecraft.world.flag;\npublic class FeatureFlagSet { public static FeatureFlagSet of() { return new FeatureFlagSet(); } }\n",
            ),
            (
                "net/minecraft/resources/ResourceKey.java",
                "package net.minecraft.resources;\npublic class ResourceKey { public ResourceKey() {} }\n",
            ),
        ] {
            write(relative, source);
        }
        write("ItemGroupProbe.java", probe);
        fs::create_dir_all(&classes).expect("create classes directory");
        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let javac = jdk_bin
            .as_ref()
            .map(|bin| bin.join("javac"))
            .unwrap_or_else(|| PathBuf::from("javac"));
        let Ok(javac_status) = Command::new(&javac)
            .arg("-nowarn")
            .arg("-d")
            .arg(&classes)
            .args(&files)
            .status()
        else {
            let _ = fs::remove_dir_all(&root);
            return;
        };
        assert!(
            javac_status.success(),
            "javac rejected the generated item-group façade"
        );
        let java = jdk_bin
            .as_ref()
            .map(|bin| bin.join("java"))
            .unwrap_or_else(|| PathBuf::from("java"));
        let output = Command::new(&java)
            .arg("-cp")
            .arg(&classes)
            .arg("ItemGroupProbe")
            .output()
            .expect("run the item-group probe");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "item-group probe failed: {stderr}");
        assert!(
            stdout.contains("horizon item group facade ok"),
            "probe output: {stdout}{stderr}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Every attachment method keeps its API descriptor, and emission stays
    /// guarded: the real class wins, an unresolved role is reported, and a
    /// resolved one is written at the API's path. The two roles are resolved
    /// from the game jar and the dependency jars, never invented.
    #[test]
    fn attachment_facade_shapes_are_guarded_and_resolved() {
        for (template, fragments) in [
            (
                FABRIC_ATTACHMENT_TYPE,
                &["public interface AttachmentType<A> {", "{identifier} id();"][..],
            ),
            (
                FABRIC_ATTACHMENT_REGISTRY,
                &[
                    "public static <A> Builder<A> create() { return new Builder<>(); }",
                    "public Builder<A> initializer(java.util.function.Supplier<A> initializer) {",
                    "public Builder<A> persistent({codec}<A> codec) {",
                    "public Builder<A> copyOnDeath() {",
                    "public AttachmentType<A> buildAndRegister({identifier} id) {",
                    "public static Registration horizonRegistration(String id) {",
                    "public static java.util.Map<String, Registration> horizonRegistrations() {",
                ][..],
            ),
            (
                FABRIC_ATTACHMENT_TARGET_SOURCE,
                &[
                    "default <A> A getAttached(AttachmentType<A> type)",
                    "default <A> boolean hasAttached(AttachmentType<A> type)",
                    "default <A> A getAttachedOrElse(AttachmentType<A> type, A defaultValue)",
                    "default <A> A getAttachedOrCreate(AttachmentType<A> type)",
                    "default <A> A getAttachedOrSet(AttachmentType<A> type, A defaultValue)",
                    "default <A> A setAttached(AttachmentType<A> type, A value)",
                    "default <A> A removeAttached(AttachmentType<A> type)",
                    "default <A> void modifyAttached(AttachmentType<A> type, java.util.function.UnaryOperator<A> modifier)",
                ][..],
            ),
            (
                NEOFORGE_ATTACHMENT_TYPE_SOURCE,
                &[
                    "public static <T> Builder<T> builder(java.util.function.Supplier<T> defaultValueSupplier) {",
                    "public Builder<T> serialize({codec}<T> codec, java.util.function.Supplier<T> defaultSupplier) {",
                    "public Builder<T> copyOnDeath() {",
                    "public AttachmentType<T> build() {",
                    "public AttachmentType<T> horizonRegister({identifier} id) {",
                ][..],
            ),
        ] {
            for fragment in fragments {
                assert!(
                    template.contains(fragment),
                    "missing attachment fragment: {fragment}"
                );
            }
            assert!(
                !template.contains("net.minecraft."),
                "hard-coded game type in an attachment template"
            );
        }
        for role in ["identifier", "codec"] {
            assert!(
                ATTACHMENT_TYPE_CANDIDATES
                    .iter()
                    .any(|(name, _)| *name == role),
                "missing attachment role: {role}"
            );
        }
        // The game jar wins; a class the jars do not carry stays unresolved.
        let game: BTreeMap<String, Class> = BTreeMap::from([(
            "net/minecraft/resources/Identifier".to_owned(),
            Class {
                name: "net/minecraft/resources/Identifier".to_owned(),
                ..Class::default()
            },
        )]);
        let libs = BTreeSet::from(["com/mojang/serialization/Codec".to_owned()]);
        let mut wanted = BTreeSet::new();
        let resolved = resolve_attachment_types(&game, &libs, &BTreeSet::new(), &mut wanted);
        assert_eq!(resolved["identifier"], "net/minecraft/resources/Identifier");
        assert_eq!(resolved["codec"], "com/mojang/serialization/Codec");
        assert!(wanted.contains("net/minecraft/resources/Identifier"));
        // A dependency class is already on the classpath, so it is not stubbed.
        assert!(!wanted.contains("com/mojang/serialization/Codec"));
        // The same holds for a dependency the target jars bundle themselves.
        let bundled = BTreeSet::from(["com/mojang/serialization/Codec"]);
        let mut wanted = BTreeSet::new();
        let resolved = resolve_attachment_types(&game, &BTreeSet::new(), &bundled, &mut wanted);
        assert_eq!(resolved["codec"], "com/mojang/serialization/Codec");
        assert!(!wanted.contains("com/mojang/serialization/Codec"));
        // The name 26.3 replaced still resolves for a jar built against it.
        let renamed: BTreeMap<String, Class> = BTreeMap::from([(
            "net/minecraft/resources/ResourceLocation".to_owned(),
            Class {
                name: "net/minecraft/resources/ResourceLocation".to_owned(),
                ..Class::default()
            },
        )]);
        let mut wanted = BTreeSet::new();
        let resolved =
            resolve_attachment_types(&renamed, &BTreeSet::new(), &BTreeSet::new(), &mut wanted);
        assert_eq!(
            resolved["identifier"],
            "net/minecraft/resources/ResourceLocation"
        );
        assert!(!resolved.contains_key("codec"));
        let mut wanted = BTreeSet::new();
        assert!(
            resolve_attachment_types(
                &BTreeMap::new(),
                &BTreeSet::new(),
                &BTreeSet::new(),
                &mut wanted
            )
            .is_empty()
        );
        assert!(wanted.is_empty());

        // Emission: an available class is never shadowed, a missing role is
        // reported, and an emitted shim lands at the API's path.
        let root = std::env::temp_dir().join(format!(
            "horizon-facade-attachment-shim-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create shim directory");
        let types = BTreeMap::from([(
            "identifier",
            "net/minecraft/resources/Identifier".to_owned(),
        )]);
        let mut files = Vec::new();
        let mut report = Report::default();
        emit_role_shim(
            &mut files,
            &mut report,
            &root,
            &BTreeSet::from([FABRIC_ATTACHMENT_TARGET.to_owned()]),
            &types,
            FABRIC_ATTACHMENT_TARGET,
            &[],
            || "shadowed".to_owned(),
        )
        .expect("guard the real API class");
        assert!(files.is_empty() && report.missing.is_empty());
        emit_role_shim(
            &mut files,
            &mut report,
            &root,
            &BTreeSet::new(),
            &types,
            "net/fabricmc/fabric/api/attachment/v1/AttachmentRegistry",
            &["identifier", "codec"],
            || "invented".to_owned(),
        )
        .expect("report a missing role");
        assert!(files.is_empty());
        assert_eq!(
            report.missing,
            vec!["net/fabricmc/fabric/api/attachment/v1/AttachmentRegistry (needs codec)"]
        );
        emit_role_shim(
            &mut files,
            &mut report,
            &root,
            &BTreeSet::new(),
            &types,
            "net/fabricmc/fabric/api/attachment/v1/AttachmentType",
            &["identifier"],
            || fill_types(FABRIC_ATTACHMENT_TYPE, &types),
        )
        .expect("emit the attachment type");
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("net/fabricmc/fabric/api/attachment/v1/AttachmentType.java"));
        let _ = fs::remove_dir_all(&root);

        // Detection and emission markers stay in the generator path.
        let source = include_str!("facade.rs");
        for marker in [
            "let uses_fabric_attachments =",
            "let uses_neoforge_attachments = uses_neoforge",
            "resolve_attachment_types(&game, &lib_names, &own, &mut wanted)",
            "if available.contains(class) {",
            "FABRIC_ATTACHMENT_TARGETS",
            "emit_role_shim(",
        ] {
            assert!(source.contains(marker), "missing guard: {marker}");
        }
        for class in [
            "net/fabricmc/fabric/api/attachment/v1/AttachmentRegistry",
            "net/fabricmc/fabric/api/attachment/v1/AttachmentType",
            FABRIC_ATTACHMENT_TARGET,
            "net/neoforged/neoforge/attachment/AttachmentType",
        ] {
            assert!(
                source.contains(&format!("\"{class}\"")),
                "attachment shim not registered: {class}"
            );
        }
        // Fabric's game targets are the classes its Mixin attaches to.
        for target in FABRIC_ATTACHMENT_TARGETS {
            assert!(
                source.contains(&format!("\"{target}\"")),
                "attachment target missing: {target}"
            );
        }
    }

    /// Fabric's attachment registry, type and target store are compiled and run
    /// for real: one retained row per id with the builder's flags, the id
    /// accessor, and the per-target store. Skipped cleanly when no JDK is
    /// installed.
    #[test]
    fn fabric_attachment_registry_compiles_and_retains_flags() {
        use std::{path::PathBuf, process::Command};
        let types: BTreeMap<&'static str, String> = BTreeMap::from([
            (
                "identifier",
                "net/minecraft/resources/Identifier".to_owned(),
            ),
            ("codec", "com/mojang/serialization/Codec".to_owned()),
        ]);
        let probe = r#"public class AttachmentProbe {
    static final class Data {
        final String value;
        Data(String value) { this.value = value; }
    }

    static final class Holder implements net.fabricmc.fabric.api.attachment.v1.AttachmentTarget {}

    public static void main(String[] args) {
        net.minecraft.resources.Identifier first = new net.minecraft.resources.Identifier("probe", "first");
        net.minecraft.resources.Identifier second = new net.minecraft.resources.Identifier("probe", "second");

        net.fabricmc.fabric.api.attachment.v1.AttachmentRegistry.Builder<Data> builder =
            net.fabricmc.fabric.api.attachment.v1.AttachmentRegistry.create();
        net.fabricmc.fabric.api.attachment.v1.AttachmentType<Data> firstType = builder
            .initializer(() -> new Data("default"))
            .persistent(new com.mojang.serialization.Codec<Data>() {})
            .copyOnDeath()
            .buildAndRegister(first);
        net.fabricmc.fabric.api.attachment.v1.AttachmentRegistry.Builder<Data> plain =
            net.fabricmc.fabric.api.attachment.v1.AttachmentRegistry.create();
        net.fabricmc.fabric.api.attachment.v1.AttachmentType<Data> secondType = plain.buildAndRegister(second);

        if (firstType.id() != first) throw new AssertionError("first id accessor");
        if (secondType.id() != second) throw new AssertionError("second id accessor");

        java.util.Map<String, net.fabricmc.fabric.api.attachment.v1.AttachmentRegistry.Registration> rows =
            net.fabricmc.fabric.api.attachment.v1.AttachmentRegistry.horizonRegistrations();
        if (!rows.keySet().toString().equals("[probe:first, probe:second]")) throw new AssertionError("id order " + rows.keySet());
        net.fabricmc.fabric.api.attachment.v1.AttachmentRegistry.Registration kept = rows.get("probe:first");
        if (kept == null || !kept.id().equals("probe:first")) throw new AssertionError("kept row");
        if (!kept.hasInitializer() || !kept.hasCodec() || !kept.persistent() || !kept.copyOnDeath()) throw new AssertionError("kept flags");
        net.fabricmc.fabric.api.attachment.v1.AttachmentRegistry.Registration unset = rows.get("probe:second");
        if (unset == null || unset.hasInitializer() || unset.hasCodec() || unset.persistent() || unset.copyOnDeath()) throw new AssertionError("unset flags");
        if (net.fabricmc.fabric.api.attachment.v1.AttachmentRegistry.horizonRegistration("probe:missing") != null) throw new AssertionError("missing row");

        try {
            net.fabricmc.fabric.api.attachment.v1.AttachmentRegistry.Builder<Data> duplicate =
                net.fabricmc.fabric.api.attachment.v1.AttachmentRegistry.create();
            duplicate.buildAndRegister(first);
            throw new AssertionError("duplicate id retained");
        } catch (IllegalStateException expected) {}
        try {
            net.fabricmc.fabric.api.attachment.v1.AttachmentRegistry.Builder<Data> missing =
                net.fabricmc.fabric.api.attachment.v1.AttachmentRegistry.create();
            missing.buildAndRegister(null);
            throw new AssertionError("null id retained");
        } catch (NullPointerException expected) {}

        Holder holder = new Holder();
        if (holder.hasAttached(firstType)) throw new AssertionError("fresh target");
        Data value = new Data("v");
        if (holder.setAttached(firstType, value) != null) throw new AssertionError("first set");
        if (holder.getAttached(firstType) != value) throw new AssertionError("round trip");
        if (holder.getAttachedOrElse(secondType, value) != value) throw new AssertionError("orElse");
        if (holder.getAttachedOrCreate(secondType) != null) throw new AssertionError("bounded create");
        Data created = holder.getAttachedOrSet(secondType, new Data("s"));
        if (!holder.hasAttached(secondType) || holder.getAttached(secondType) != created) throw new AssertionError("orSet");
        holder.modifyAttached(secondType, previous -> new Data(previous.value + "!"));
        if (!holder.getAttached(secondType).value.equals("s!")) throw new AssertionError("modify " + holder.getAttached(secondType).value);
        if (holder.removeAttached(secondType) == null || holder.hasAttached(secondType)) throw new AssertionError("remove");
        if (!holder.getAttachedOrGet(secondType, () -> new Data("g")).value.equals("g")) throw new AssertionError("orGet");

        // NeoForge keeps the same retention shape: builder markers, flags, and
        // the registration id it was handed.
        net.minecraft.resources.Identifier neoId = new net.minecraft.resources.Identifier("probe", "neo");
        net.neoforged.neoforge.attachment.AttachmentType<Data> neo =
            net.neoforged.neoforge.attachment.AttachmentType.<Data>builder(() -> new Data("neo-default"))
                .serialize(new com.mojang.serialization.Codec<Data>() {}, () -> new Data("neo-serialized"))
                .copyOnDeath()
                .build()
                .horizonRegister(neoId);
        if (neo.horizonId() != neoId) throw new AssertionError("neo id");
        if (!neo.hasCodec() || !neo.persistent() || !neo.copyOnDeath()) throw new AssertionError("neo flags");
        if (!neo.defaultValue().value.equals("neo-serialized")) throw new AssertionError("neo default");
        if (net.neoforged.neoforge.attachment.AttachmentType.horizonRegistered("probe:neo") != neo) throw new AssertionError("neo registration");
        System.out.println("horizon attachment facade ok");
    }
}
"#;
        let root =
            std::env::temp_dir().join(format!("horizon-facade-attachment-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        let mut files = Vec::new();
        let mut write = |relative: &str, text: &str| {
            let file = src.join(relative);
            fs::create_dir_all(file.parent().expect("package directory")).expect("create package");
            fs::write(&file, text).expect("write java shim");
            files.push(file);
        };
        for (relative, template) in [
            ("AttachmentType.java", FABRIC_ATTACHMENT_TYPE),
            ("AttachmentRegistry.java", FABRIC_ATTACHMENT_REGISTRY),
            ("AttachmentTarget.java", FABRIC_ATTACHMENT_TARGET_SOURCE),
        ] {
            write(
                &format!("net/fabricmc/fabric/api/attachment/v1/{relative}"),
                &fill_types(template, &types),
            );
        }
        write(
            "net/neoforged/neoforge/attachment/AttachmentType.java",
            &fill_types(NEOFORGE_ATTACHMENT_TYPE_SOURCE, &types),
        );
        // The smallest game types the attachment signatures name.
        for (relative, source) in [
            (
                "net/minecraft/resources/Identifier.java",
                "package net.minecraft.resources;\npublic class Identifier { private final String namespace; private final String path; public Identifier(String namespace, String path) { this.namespace = namespace; this.path = path; } public String toString() { return namespace + \":\" + path; } }\n",
            ),
            (
                "com/mojang/serialization/Codec.java",
                "package com.mojang.serialization;\npublic interface Codec<A> {}\n",
            ),
        ] {
            write(relative, source);
        }
        write("AttachmentProbe.java", probe);
        fs::create_dir_all(&classes).expect("create classes directory");
        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let javac = jdk_bin
            .as_ref()
            .map(|bin| bin.join("javac"))
            .unwrap_or_else(|| PathBuf::from("javac"));
        let Ok(javac_status) = Command::new(&javac)
            .arg("-nowarn")
            .arg("-d")
            .arg(&classes)
            .args(&files)
            .status()
        else {
            let _ = fs::remove_dir_all(&root);
            return;
        };
        assert!(
            javac_status.success(),
            "javac rejected the generated attachment façade"
        );
        let java = jdk_bin
            .as_ref()
            .map(|bin| bin.join("java"))
            .unwrap_or_else(|| PathBuf::from("java"));
        let output = Command::new(&java)
            .arg("-cp")
            .arg(&classes)
            .arg("AttachmentProbe")
            .output()
            .expect("run the attachment probe");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "attachment probe failed: {stderr}");
        assert!(
            stdout.contains("horizon attachment facade ok"),
            "probe output: {stdout}{stderr}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Fabric's and NeoForge's menu façades: every template names a role instead
    /// of a game type, resolution takes the game jar before a library before the
    /// target's own classes, a real API class is never shadowed, a missing role
    /// is reported, and the generated `MenuType` stub implements the façade's
    /// own `IMenuTypeExtension` only when that shim is ours.
    #[test]
    fn menu_facade_shapes_are_guarded_and_resolved() {
        for (template, fragments) in [
            (
                FABRIC_MENU_TYPE_SOURCE,
                &[
                    "public class {name}<T extends {menu}, D> extends {menu_type}",
                    "public {name}(ExtendedFactory<T, D> factory, {codec} codec)",
                    "super(null, null)",
                    "public T create(int windowId, {inventory} inventory, D data)",
                    "public {codec} getCodec()",
                    "public {codec} getStreamCodec()",
                    "public {menu_type} getMenuType() { return this; }",
                    "public interface ExtendedFactory<T extends {menu}, D>",
                ][..],
            ),
            (
                FABRIC_MENU_FACTORY_SOURCE,
                &[
                    "public interface {name}<D> extends {menu_provider}",
                    "D getScreenOpeningData({server_player} player)",
                    "{component} getDisplayName();",
                    "default {screen} createScreen(int windowId, {inventory} inventory, {component} title)",
                ][..],
            ),
            (
                NEOFORGE_CONTAINER_FACTORY_SOURCE,
                &[
                    "public interface IContainerFactory<T extends {menu}>",
                    "T create(int windowId, {inventory} inventory, {byte_buf} data)",
                    "default T create(int windowId, {inventory} inventory)",
                ][..],
            ),
            (
                NEOFORGE_EXTENDED_CONTAINER_FACTORY_SOURCE,
                &[
                    "public interface IExtendedContainerFactory<T extends {menu}> extends IContainerFactory<T>",
                ][..],
            ),
            (
                NEOFORGE_MENU_TYPE_EXTENSION_SOURCE,
                &[
                    "static <T extends {menu}> {menu_type} create(net.neoforged.neoforge.network.IContainerFactory<T> factory)",
                    "static <T extends {menu}> {menu_type} create({identifier} id,",
                    "new {menu_type}(null, null) { }",
                    "HORIZON_FACTORIES.put(String.valueOf(id), factory);",
                    "HORIZON_IDS.put(type, id);",
                    "default {identifier} naturalId() { return HORIZON_IDS.get(this); }",
                ][..],
            ),
        ] {
            for fragment in fragments {
                assert!(
                    template.contains(fragment),
                    "missing menu fragment: {fragment}"
                );
            }
            assert!(
                !template.contains("net.minecraft."),
                "hard-coded game type in a menu template"
            );
        }
        for role in [
            "menu",
            "menu_type",
            "menu_provider",
            "inventory",
            "byte_buf",
            "codec",
            "identifier",
            "component",
            "server_player",
            "screen",
        ] {
            assert!(
                MENU_TYPE_CANDIDATES.iter().any(|(name, _)| *name == role),
                "missing menu role: {role}"
            );
        }
        let game = |names: &[&str]| -> BTreeMap<String, Class> {
            names
                .iter()
                .map(|name| {
                    (
                        (*name).to_owned(),
                        Class {
                            name: (*name).to_owned(),
                            ..Class::default()
                        },
                    )
                })
                .collect()
        };
        let none: BTreeSet<String> = BTreeSet::new();
        let no_own: BTreeSet<&str> = BTreeSet::new();
        // 26.3's renamed types win over the names they replaced, and the game
        // classes a role resolves to are seeded as the stubs the shims compile
        // against.
        let jar = game(&[
            "net/minecraft/world/inventory/AbstractContainerMenu",
            "net/minecraft/world/inventory/MenuType",
            "net/minecraft/network/RegistryFriendlyByteBuf",
            "net/minecraft/network/FriendlyByteBuf",
            "net/minecraft/network/codec/StreamCodec",
            "net/minecraft/resources/Identifier",
        ]);
        let mut wanted = BTreeSet::new();
        let resolved = resolve_menu_types(&jar, &none, &no_own, &mut wanted);
        assert_eq!(
            resolved["menu"],
            "net/minecraft/world/inventory/AbstractContainerMenu"
        );
        assert_eq!(
            resolved["menu_type"],
            "net/minecraft/world/inventory/MenuType"
        );
        assert_eq!(
            resolved["byte_buf"],
            "net/minecraft/network/RegistryFriendlyByteBuf"
        );
        assert_eq!(resolved["codec"], "net/minecraft/network/codec/StreamCodec");
        assert!(wanted.contains("net/minecraft/world/inventory/MenuType"));
        assert!(!wanted.contains("com/mojang/serialization/Codec"));
        // A dependency class resolves the codec role and is not stubbed.
        let libs = BTreeSet::from(["com/mojang/serialization/Codec".to_owned()]);
        let mut wanted = BTreeSet::new();
        let resolved = resolve_menu_types(&game(&[]), &libs, &no_own, &mut wanted);
        assert_eq!(resolved["codec"], "com/mojang/serialization/Codec");
        assert!(wanted.is_empty());
        // The name 26.3 replaced still resolves for a jar built against it.
        let legacy = game(&["net/minecraft/resources/ResourceLocation"]);
        let mut wanted = BTreeSet::new();
        assert_eq!(
            resolve_menu_types(&legacy, &none, &no_own, &mut wanted)["identifier"],
            "net/minecraft/resources/ResourceLocation"
        );
        // A role the target jars carry themselves resolves but is not a stub.
        let own = BTreeSet::from(["net/minecraft/world/entity/player/Inventory"]);
        let mut wanted = BTreeSet::new();
        assert_eq!(
            resolve_menu_types(&game(&[]), &none, &own, &mut wanted)["inventory"],
            "net/minecraft/world/entity/player/Inventory"
        );
        assert!(wanted.is_empty());
        // Nothing on this classpath resolves a role, so nothing is invented.
        let mut wanted = BTreeSet::new();
        assert!(resolve_menu_types(&game(&[]), &none, &no_own, &mut wanted).is_empty());
        assert!(wanted.is_empty());

        // Emission: an available class is never shadowed, a missing role is
        // reported, and an emitted shim lands at the API's path with every role
        // filled in.
        let root = std::env::temp_dir().join(format!("horizon-facade-menu-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create shim directory");
        let types = BTreeMap::from([
            (
                "menu",
                "net/minecraft/world/inventory/AbstractContainerMenu".to_owned(),
            ),
            (
                "menu_type",
                "net/minecraft/world/inventory/MenuType".to_owned(),
            ),
            (
                "inventory",
                "net/minecraft/world/entity/player/Inventory".to_owned(),
            ),
            (
                "codec",
                "net/minecraft/network/codec/StreamCodec".to_owned(),
            ),
        ]);
        let mut files = Vec::new();
        let mut report = Report::default();
        emit_role_shim(
            &mut files,
            &mut report,
            &root,
            &BTreeSet::from([FABRIC_MENU_TYPE.to_owned()]),
            &types,
            FABRIC_MENU_TYPE,
            &[],
            || "shadowed".to_owned(),
        )
        .expect("guard the real API class");
        assert!(files.is_empty() && report.missing.is_empty());
        emit_role_shim(
            &mut files,
            &mut report,
            &root,
            &BTreeSet::new(),
            &types,
            FABRIC_SCREEN_HANDLER_TYPE,
            &["menu", "menu_type", "inventory", "codec", "screen"],
            || "invented".to_owned(),
        )
        .expect("report a missing role");
        assert!(files.is_empty());
        assert_eq!(
            report.missing,
            vec![format!("{FABRIC_SCREEN_HANDLER_TYPE} (needs screen)")]
        );
        let mut named = types.clone();
        named.insert("package", "net/fabricmc/fabric/api/menu/v1".to_owned());
        named.insert("name", "ExtendedMenuType".to_owned());
        emit_role_shim(
            &mut files,
            &mut report,
            &root,
            &BTreeSet::new(),
            &types,
            FABRIC_MENU_TYPE,
            &["menu", "menu_type", "inventory", "codec"],
            || fill_types(FABRIC_MENU_TYPE_SOURCE, &named),
        )
        .expect("emit the extended menu type");
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("net/fabricmc/fabric/api/menu/v1/ExtendedMenuType.java"));
        let written = fs::read_to_string(&files[0]).expect("read the emitted shim");
        assert!(written.contains("public class ExtendedMenuType<T extends"));
        assert!(written.contains("package net.fabricmc.fabric.api.menu.v1;"));
        for role in named.keys() {
            assert!(
                !written.contains(&format!("{{{role}}}")),
                "unfilled menu role: {role}"
            );
        }
        let _ = fs::remove_dir_all(&root);

        // Detection and emission markers stay in the generator path, and only
        // the façade's own extension is implemented by the generated stub.
        let source = include_str!("facade.rs");
        for marker in [
            "let uses_fabric_menus = names_any",
            "let uses_neoforge_menus = uses_neoforge",
            "resolve_menu_types(&game, &lib_names, &own, &mut wanted)",
            "let menu_type_extension =",
            "if menu_type_extension && let Some(menu_type) = menu_types.get(\"menu_type\")",
            "if !names_any(&[class]) {",
        ] {
            assert!(source.contains(marker), "missing guard: {marker}");
        }
        for class in [
            FABRIC_MENU_TYPE,
            FABRIC_MENU_FACTORY,
            FABRIC_SCREEN_HANDLER_TYPE,
            FABRIC_SCREEN_HANDLER_FACTORY,
            NEOFORGE_CONTAINER_FACTORY,
            NEOFORGE_EXTENDED_CONTAINER_FACTORY,
            NEOFORGE_MENU_TYPE_EXTENSION,
        ] {
            assert!(
                source.contains(&format!("\"{class}\"")),
                "menu shim not registered: {class}"
            );
        }
    }

    /// Fabric's extended menu type, its factory and NeoForge's `MenuType`
    /// extension are compiled and run for real: the type keeps the factory and
    /// codec marker it was registered with, the factory keeps its opening data,
    /// and the extension retains one factory per menu id. Skipped cleanly when
    /// no JDK is installed.
    #[test]
    fn fabric_extended_menu_type_compiles_and_retains_registrations() {
        use std::{path::PathBuf, process::Command};
        let types: BTreeMap<&'static str, String> = BTreeMap::from([
            (
                "menu",
                "net/minecraft/world/inventory/AbstractContainerMenu".to_owned(),
            ),
            (
                "menu_type",
                "net/minecraft/world/inventory/MenuType".to_owned(),
            ),
            (
                "menu_provider",
                "net/minecraft/world/MenuProvider".to_owned(),
            ),
            (
                "inventory",
                "net/minecraft/world/entity/player/Inventory".to_owned(),
            ),
            (
                "byte_buf",
                "net/minecraft/network/RegistryFriendlyByteBuf".to_owned(),
            ),
            (
                "codec",
                "net/minecraft/network/codec/StreamCodec".to_owned(),
            ),
            (
                "identifier",
                "net/minecraft/resources/Identifier".to_owned(),
            ),
            (
                "component",
                "net/minecraft/network/chat/Component".to_owned(),
            ),
            (
                "server_player",
                "net/minecraft/server/level/ServerPlayer".to_owned(),
            ),
            (
                "screen",
                "net/minecraft/client/gui/screens/Screen".to_owned(),
            ),
        ]);
        let probe = r#"public class MenuProbe {
    static final class Crate extends net.minecraft.world.inventory.AbstractContainerMenu {
        final int windowId;
        Crate(int windowId, Object data) { super(windowId); this.windowId = windowId; }
    }

    static final class Data { final String value; Data(String value) { this.value = value; } }

    static final class Provider implements net.fabricmc.fabric.api.screenhandler.v1.ExtendedScreenHandlerFactory<Data> {
        public net.minecraft.network.chat.Component getDisplayName() { return () -> "crate"; }
        public Data getScreenOpeningData(net.minecraft.server.level.ServerPlayer player) { return new Data("opened"); }
    }

    public static void main(String[] args) {
        net.minecraft.network.codec.StreamCodec<net.minecraft.network.RegistryFriendlyByteBuf, Data> codec =
            new net.minecraft.network.codec.StreamCodec<net.minecraft.network.RegistryFriendlyByteBuf, Data>() {};
        net.fabricmc.fabric.api.screenhandler.v1.ExtendedScreenHandlerType.ExtendedFactory<Crate, Data> factory =
            (windowId, inventory, data) -> new Crate(windowId, data);
        net.fabricmc.fabric.api.screenhandler.v1.ExtendedScreenHandlerType<Crate, Data> type =
            new net.fabricmc.fabric.api.screenhandler.v1.ExtendedScreenHandlerType<>(factory, codec);
        if (type.getMenuType() != type) throw new AssertionError("wrapped menu type");
        if (type.getCodec() != codec || type.getStreamCodec() != codec) throw new AssertionError("codec marker");
        Crate menu = type.create(7, new net.minecraft.world.entity.player.Inventory(), new Data("d"));
        if (menu == null || menu.windowId != 7) throw new AssertionError("factory route");

        Provider provider = new Provider();
        if (!provider.getScreenOpeningData(null).value.equals("opened")) throw new AssertionError("opening data");
        if (!provider.getDisplayName().getString().equals("crate")) throw new AssertionError("display name");
        if (provider.createScreen(1, new net.minecraft.world.entity.player.Inventory(), provider.getDisplayName()) != null) throw new AssertionError("bounded screen");

        net.minecraft.resources.Identifier id = new net.minecraft.resources.Identifier("probe", "crate");
        net.minecraft.world.inventory.MenuType neo =
            net.neoforged.neoforge.common.extensions.IMenuTypeExtension.create(id, (windowId, inventory, data) -> new Crate(windowId, data));
        if (neo == null) throw new AssertionError("neo type");
        if (((net.neoforged.neoforge.common.extensions.IMenuTypeExtension) neo).naturalId() != id) throw new AssertionError("natural id");
        if (net.neoforged.neoforge.common.extensions.IMenuTypeExtension.horizonFactory("probe:crate") == null) throw new AssertionError("retained factory");
        if (net.neoforged.neoforge.common.extensions.IMenuTypeExtension.horizonFactories().size() != 1) throw new AssertionError("factory count");
        System.out.println("horizon menu facade ok");
    }
}
"#;
        let root =
            std::env::temp_dir().join(format!("horizon-facade-menu-shim-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        let mut files = Vec::new();
        let mut write = |relative: &str, text: &str| {
            let file = src.join(relative);
            fs::create_dir_all(file.parent().expect("package directory")).expect("create package");
            fs::write(&file, text).expect("write java shim");
            files.push(file);
        };
        // The 1.20.x Fabric names, so the renamed template's `{name}` role is
        // exercised as well as the class lookup.
        let named = |class: &str| {
            let (package, simple) = class.rsplit_once('/').expect("package and name");
            let mut named = types.clone();
            named.insert("package", package.to_owned());
            named.insert("name", simple.to_owned());
            named
        };
        write(
            "net/fabricmc/fabric/api/screenhandler/v1/ExtendedScreenHandlerType.java",
            &fill_types(
                FABRIC_MENU_TYPE_SOURCE,
                &named("net/fabricmc/fabric/api/screenhandler/v1/ExtendedScreenHandlerType"),
            ),
        );
        write(
            "net/fabricmc/fabric/api/screenhandler/v1/ExtendedScreenHandlerFactory.java",
            &fill_types(
                FABRIC_MENU_FACTORY_SOURCE,
                &named("net/fabricmc/fabric/api/screenhandler/v1/ExtendedScreenHandlerFactory"),
            ),
        );
        for (relative, template) in [
            ("IContainerFactory.java", NEOFORGE_CONTAINER_FACTORY_SOURCE),
            (
                "IExtendedContainerFactory.java",
                NEOFORGE_EXTENDED_CONTAINER_FACTORY_SOURCE,
            ),
        ] {
            write(
                &format!("net/neoforged/neoforge/network/{relative}"),
                &fill_types(template, &types),
            );
        }
        write(
            "net/neoforged/neoforge/common/extensions/IMenuTypeExtension.java",
            &fill_types(NEOFORGE_MENU_TYPE_EXTENSION_SOURCE, &types),
        );
        // The smallest game types the menu signatures name, plus the `MenuType`
        // stub the generator emits: it implements the extension, so
        // `naturalId()` lands on the anonymous type `create` builds.
        for (relative, source) in [
            (
                "net/minecraft/world/inventory/AbstractContainerMenu.java",
                "package net.minecraft.world.inventory;\npublic class AbstractContainerMenu { public final int containerId; public AbstractContainerMenu(int containerId) { this.containerId = containerId; } }\n",
            ),
            (
                "net/minecraft/world/inventory/MenuType.java",
                "package net.minecraft.world.inventory;\npublic class MenuType<T extends AbstractContainerMenu> implements net.neoforged.neoforge.common.extensions.IMenuTypeExtension {\n    public MenuType() {}\n    protected MenuType(Object supplier, Object requiredFeatures) {}\n    public T create(int windowId, net.minecraft.world.entity.player.Inventory inventory) { return null; }\n}\n",
            ),
            (
                "net/minecraft/world/entity/player/Inventory.java",
                "package net.minecraft.world.entity.player;\npublic class Inventory {}\n",
            ),
            (
                "net/minecraft/world/MenuProvider.java",
                "package net.minecraft.world;\npublic interface MenuProvider { net.minecraft.network.chat.Component getDisplayName(); }\n",
            ),
            (
                "net/minecraft/network/chat/Component.java",
                "package net.minecraft.network.chat;\npublic interface Component { String getString(); }\n",
            ),
            (
                "net/minecraft/network/RegistryFriendlyByteBuf.java",
                "package net.minecraft.network;\npublic class RegistryFriendlyByteBuf {}\n",
            ),
            (
                "net/minecraft/network/codec/StreamCodec.java",
                "package net.minecraft.network.codec;\npublic interface StreamCodec<B, V> {}\n",
            ),
            (
                "net/minecraft/server/level/ServerPlayer.java",
                "package net.minecraft.server.level;\npublic class ServerPlayer {}\n",
            ),
            (
                "net/minecraft/client/gui/screens/Screen.java",
                "package net.minecraft.client.gui.screens;\npublic class Screen {}\n",
            ),
            (
                "net/minecraft/resources/Identifier.java",
                "package net.minecraft.resources;\npublic class Identifier { private final String namespace; private final String path; public Identifier(String namespace, String path) { this.namespace = namespace; this.path = path; } public String toString() { return namespace + \":\" + path; } }\n",
            ),
        ] {
            write(relative, source);
        }
        write("MenuProbe.java", probe);
        fs::create_dir_all(&classes).expect("create classes directory");
        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let javac = jdk_bin
            .as_ref()
            .map(|bin| bin.join("javac"))
            .unwrap_or_else(|| PathBuf::from("javac"));
        let Ok(javac_status) = Command::new(&javac)
            .arg("-nowarn")
            .arg("-d")
            .arg(&classes)
            .args(&files)
            .status()
        else {
            let _ = fs::remove_dir_all(&root);
            return;
        };
        assert!(
            javac_status.success(),
            "javac rejected the generated menu façade"
        );
        let java = jdk_bin
            .as_ref()
            .map(|bin| bin.join("java"))
            .unwrap_or_else(|| PathBuf::from("java"));
        let output = Command::new(&java)
            .arg("-cp")
            .arg(&classes)
            .arg("MenuProbe")
            .output()
            .expect("run the menu probe");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "menu probe failed: {stderr}");
        assert!(
            stdout.contains("horizon menu facade ok"),
            "probe output: {stdout}{stderr}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn model_loading_facade_shapes_match_the_api() {
        // Exact descriptors: the provider interfaces are the lambda targets a
        // mod compiles against, and the registry keeps a `{kind, id, provider}`
        // ledger in call order.
        for (template, fragment) in [
            (
                FABRIC_MODEL_PROVIDER_CONTEXT_SOURCE,
                "default {baked_model} loadModel({identifier} id) {",
            ),
            (
                FABRIC_MODEL_PROVIDER_CONTEXT_SOURCE,
                "default {baked_model} loadModel({identifier} id, {model_loader} loader) {",
            ),
            (
                FABRIC_MODEL_RESOURCE_PROVIDER_SOURCE,
                "{unbaked_model} loadModelResource({identifier} id, ModelProviderContext context);",
            ),
            (
                FABRIC_MODEL_VARIANT_PROVIDER_SOURCE,
                "{unbaked_model} loadModelVariant({identifier} id, ModelProviderContext context);",
            ),
            (
                FABRIC_MODEL_APPENDER_SOURCE,
                "void appendAll(ModelProviderContext context, Consumer<{identifier}> consumer);",
            ),
            (
                FABRIC_MODEL_LOADING_REGISTRY_SOURCE,
                "HORIZON_REGISTRATIONS.add(new Object[] { kind, id, provider });",
            ),
            (
                NEOFORGE_MODEL_EVENT_SOURCE,
                "public static class RegisterAdditional extends ModelEvent {",
            ),
            (
                NEOFORGE_MODEL_EVENT_SOURCE,
                "public void horizonRegister({model_key} location, {baked_model} model) {",
            ),
        ] {
            assert!(
                template.contains(fragment),
                "missing model fragment: {fragment}"
            );
            assert!(
                !template.contains("net.minecraft."),
                "hard-coded game type in a model template"
            );
        }
        // The roles resolve against the game jar, 26.3's dispatch name first;
        // the class it replaced still binds for an older jar.
        let game = |names: &[&str]| -> BTreeMap<String, Class> {
            names
                .iter()
                .map(|name| ((*name).to_owned(), Class::default()))
                .collect()
        };
        let none = BTreeSet::new();
        let modern = resolve_client_types(
            &game(&[
                "net/minecraft/resources/Identifier",
                "net/minecraft/client/resources/model/BakedModel",
                "net/minecraft/client/resources/model/UnbakedModel",
                "net/minecraft/client/resources/model/ModelLoader",
                "net/minecraft/client/renderer/block/dispatch/ModelState",
            ]),
            &none,
            &none,
        );
        assert_eq!(
            modern["model_key"],
            "net/minecraft/client/renderer/block/dispatch/ModelState"
        );
        assert_eq!(modern["identifier"], "net/minecraft/resources/Identifier");
        let legacy = resolve_client_types(
            &game(&["net/minecraft/client/resources/model/ModelResourceLocation"]),
            &none,
            &none,
        );
        assert_eq!(
            legacy["model_key"],
            "net/minecraft/client/resources/model/ModelResourceLocation"
        );
        // A server-shaped jar resolves no model role: the slice reports the
        // missing types instead of inventing one.
        assert!(!resolve_client_types(&game(&[]), &none, &none).contains_key("model_key"));
        // Emission is gated by the API class and by role resolution.
        let source = include_str!("facade.rs");
        for marker in [
            "let uses_fabric_models = target",
            "let uses_neoforge_models = uses_neoforge",
            "if !available.contains(class) {",
        ] {
            assert!(source.contains(marker), "missing model guard: {marker}");
        }
        // A role the jar cannot resolve is reported, not invented; a real API
        // class is never shadowed. This is the guard the NeoForge slice uses.
        let root =
            std::env::temp_dir().join(format!("horizon-facade-model-roles-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create model shim directory");
        let empty = BTreeSet::new();
        let types = BTreeMap::new();
        let shims = ClientShims {
            src: &root,
            available: &empty,
            types: &types,
        };
        let mut files = Vec::new();
        let mut report = Report::default();
        let written = shims
            .emit(
                &mut files,
                &mut report,
                FABRIC_MODEL_LOADING_REGISTRY,
                &["model_key"],
                String::new,
            )
            .expect("emit a model shim");
        assert!(!written && files.is_empty());
        assert_eq!(
            report.missing,
            vec![format!(
                "{FABRIC_MODEL_LOADING_REGISTRY} (needs client type model_key)"
            )]
        );
        let real = BTreeSet::from([FABRIC_MODEL_LOADING_REGISTRY.to_owned()]);
        let shims = ClientShims {
            src: &root,
            available: &real,
            types: &types,
        };
        let mut report = Report::default();
        let written = shims
            .emit(
                &mut files,
                &mut report,
                FABRIC_MODEL_LOADING_REGISTRY,
                &[],
                String::new,
            )
            .expect("emit over a real API class");
        assert!(!written && files.is_empty() && report.missing.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    /// The Fabric model-loading registry is compiled and run for real: retained
    /// providers, their order and the context's recorded ids are behaviour, not
    /// a shape. Skipped cleanly when no JDK is installed.
    #[test]
    fn model_loading_facade_compiles_and_retains_providers() {
        use std::{path::PathBuf, process::Command};
        let types: BTreeMap<&'static str, String> = [
            ("identifier", "net/minecraft/resources/Identifier"),
            (
                "unbaked_model",
                "net/minecraft/client/resources/model/UnbakedModel",
            ),
            (
                "baked_model",
                "net/minecraft/client/resources/model/BakedModel",
            ),
            (
                "model_loader",
                "net/minecraft/client/resources/model/ModelLoader",
            ),
        ]
        .into_iter()
        .map(|(role, name)| (role, name.to_owned()))
        .collect();
        let mut model_types = types.clone();
        model_types.insert(
            "model_provider_context",
            FABRIC_MODEL_PROVIDER_CONTEXT.to_owned(),
        );
        let probe = r#"import java.util.*;
import net.minecraft.resources.Identifier;
import net.minecraft.client.resources.model.UnbakedModel;
import net.fabricmc.fabric.api.client.model.loading.v1.*;
public class ModelProbe {
    public static void main(String[] args) {
        Identifier one = new Identifier("minecraft", "one");
        Identifier two = new Identifier("minecraft", "two");
        UnbakedModel model = new UnbakedModel();
        ModelResourceProvider resource = (id, context) -> model;
        ModelVariantProvider variant = (id, context) -> model;
        List<Identifier> appended = new ArrayList<>();
        ModelAppender appender = (context, consumer) -> { consumer.accept(one); consumer.accept(two); };
        ModelLoadingRegistry.registerModelProvider(one, resource);
        ModelLoadingRegistry.registerModelVariantProvider(variant);
        ModelLoadingRegistry.registerResourceProvider(resource);
        ModelLoadingRegistry.registerAppender(appender);
        ModelLoadingRegistry.registerResourceProvider(null);
        List<Object[]> registrations = ModelLoadingRegistry.horizonRegistrations();
        if (registrations.size() != 4) throw new AssertionError("registrations " + registrations.size());
        String[] kinds = { "model", "variant", "resource", "appender" };
        for (int i = 0; i < kinds.length; i++) if (!kinds[i].equals(registrations.get(i)[0])) throw new AssertionError("kind " + i);
        if (registrations.get(0)[1] != one || registrations.get(0)[2] != resource) throw new AssertionError("model registration");
        if (registrations.get(1)[2] != variant) throw new AssertionError("variant registration");
        if (registrations.get(3)[2] != appender) throw new AssertionError("appender registration");
        ModelProviderContext context = new ModelProviderContext() {};
        if (context.loadModel(one) != null) throw new AssertionError("no model is baked here");
        context.loadModel(two, null);
        context.loadModel(null);
        if (!ModelProviderContext.horizonRequestedModels().equals(Arrays.asList(one, two))) throw new AssertionError("requested");
        appender.appendAll(context, appended::add);
        if (appended.size() != 2 || appended.get(0) != one || appended.get(1) != two) throw new AssertionError("appended");
        System.out.println("horizon model facade ok");
    }
}
"#;
        let root =
            std::env::temp_dir().join(format!("horizon-facade-model-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        let mut files = Vec::new();
        let mut write = |relative: &str, text: &str| {
            let file = src.join(relative);
            fs::create_dir_all(file.parent().expect("package directory")).expect("create package");
            fs::write(&file, text).expect("write java shim");
            files.push(file);
        };
        for (relative, source) in [
            ("ModelProviderContext", FABRIC_MODEL_PROVIDER_CONTEXT_SOURCE),
            (
                "ModelResourceProvider",
                FABRIC_MODEL_RESOURCE_PROVIDER_SOURCE,
            ),
            ("ModelVariantProvider", FABRIC_MODEL_VARIANT_PROVIDER_SOURCE),
            ("ModelAppender", FABRIC_MODEL_APPENDER_SOURCE),
            ("ModelLoadingRegistry", FABRIC_MODEL_LOADING_REGISTRY_SOURCE),
        ] {
            write(
                &format!("net/fabricmc/fabric/api/client/model/loading/v1/{relative}.java"),
                &fill_types(source, &model_types),
            );
        }
        // The smallest game types the signatures name.
        write(
            "net/minecraft/resources/Identifier.java",
            "package net.minecraft.resources; public class Identifier { public Identifier(String namespace, String path) {} }",
        );
        write(
            "net/minecraft/client/resources/model/UnbakedModel.java",
            "package net.minecraft.client.resources.model; public class UnbakedModel {}",
        );
        write(
            "net/minecraft/client/resources/model/BakedModel.java",
            "package net.minecraft.client.resources.model; public class BakedModel {}",
        );
        write(
            "net/minecraft/client/resources/model/ModelLoader.java",
            "package net.minecraft.client.resources.model; public class ModelLoader {}",
        );
        write("ModelProbe.java", probe);
        fs::create_dir_all(&classes).expect("create classes directory");
        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let javac = jdk_bin
            .as_ref()
            .map(|bin| bin.join("javac"))
            .unwrap_or_else(|| PathBuf::from("javac"));
        let Ok(javac_status) = Command::new(&javac)
            .arg("-nowarn")
            .arg("-d")
            .arg(&classes)
            .args(&files)
            .status()
        else {
            let _ = fs::remove_dir_all(&root);
            return;
        };
        assert!(
            javac_status.success(),
            "javac rejected the generated model façade"
        );
        let java = jdk_bin
            .as_ref()
            .map(|bin| bin.join("java"))
            .unwrap_or_else(|| PathBuf::from("java"));
        let output = Command::new(&java)
            .arg("-cp")
            .arg(&classes)
            .arg("ModelProbe")
            .output()
            .expect("run the model probe");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "model probe failed: {stderr}");
        assert!(
            stdout.contains("horizon model facade ok"),
            "probe output: {stdout}{stderr}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The Fabric datagen slice's exact shapes, its resolved roles and its two
    /// guards: a real API class is never shadowed, and a role this classpath
    /// cannot resolve reports the slice instead of inventing a type.
    #[test]
    fn datagen_facade_shapes_are_guarded_and_resolved() {
        for fragment in [
            "void onInitializeDataGenerator(FabricDataGenerator fabricDataGenerator);",
            "public {mod_container} getModContainer() { return horizonModContainer; }",
            "public String getModId() { return horizonModContainer.getMetadata().getId(); }",
            "public Pack createPack() {",
            "public Pack addProvider(DataProvider provider) {",
            "public Pack addProvider(DataProvider.Factory factory) {",
            "new Object[] { horizonPack, provider.getName(), provider.getKind(), provider }",
            "DataProvider create(FabricDataOutput output);",
            "public FabricRecipeProvider(FabricDataOutput output, java.util.concurrent.CompletableFuture<?> registriesFuture) {",
            "public FabricLanguageProvider(FabricDataOutput output, String modId) {",
            "public FabricModelProvider(FabricDataOutput output) {",
        ] {
            let found = [
                FABRIC_DATAGEN_ENTRYPOINT,
                FABRIC_DATA_OUTPUT,
                FABRIC_DATA_PROVIDER,
                FABRIC_RECIPE_PROVIDER,
                FABRIC_LANGUAGE_PROVIDER,
                FABRIC_MODEL_PROVIDER,
                FABRIC_DATA_GENERATOR,
            ]
            .iter()
            .any(|template| template.contains(fragment));
            assert!(found, "missing datagen fragment: {fragment}");
        }
        for template in [
            FABRIC_DATA_OUTPUT,
            FABRIC_DATA_PROVIDER,
            FABRIC_RECIPE_PROVIDER,
            FABRIC_LANGUAGE_PROVIDER,
            FABRIC_MODEL_PROVIDER,
            FABRIC_DATA_GENERATOR,
        ] {
            assert!(
                !template.contains("net.minecraft."),
                "hard-coded game type in a datagen template"
            );
        }
        // The loader unit lands in the same run for a jar-only slice, and the
        // game jar supplies the id (26.3's name first, the one it replaced
        // second). A classpath that carries neither reports the slice.
        let game = |names: &[&str]| -> BTreeMap<String, Class> {
            names
                .iter()
                .map(|name| ((*name).to_owned(), Class::default()))
                .collect()
        };
        let none = BTreeSet::new();
        let no_own: BTreeSet<&str> = BTreeSet::new();
        let mut wanted = BTreeSet::new();
        let types = resolve_datagen_types(
            &game(&["net/minecraft/resources/Identifier"]),
            &none,
            &no_own,
            true,
            &mut wanted,
        );
        assert_eq!(
            types["mod_container"],
            "net/fabricmc/loader/api/ModContainer"
        );
        assert_eq!(types["identifier"], "net/minecraft/resources/Identifier");
        assert!(wanted.contains("net/minecraft/resources/Identifier"));
        let libs = BTreeSet::from(["net/fabricmc/loader/api/ModContainer".to_owned()]);
        let legacy = resolve_datagen_types(
            &game(&["net/minecraft/resources/ResourceLocation"]),
            &libs,
            &no_own,
            false,
            &mut BTreeSet::new(),
        );
        assert_eq!(
            legacy["identifier"],
            "net/minecraft/resources/ResourceLocation"
        );
        assert!(
            resolve_datagen_types(&game(&[]), &none, &no_own, false, &mut BTreeSet::new())
                .is_empty()
        );
        for marker in [
            "let uses_fabric_datagen = target",
            "if uses_fabric_datagen {",
            "for (class, source, requires) in FABRIC_DATAGEN_SHIMS {",
        ] {
            assert!(
                include_str!("facade.rs").contains(marker),
                "missing datagen guard: {marker}"
            );
        }
        let root =
            std::env::temp_dir().join(format!("horizon-facade-datagen-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create datagen shim directory");
        let generator = "net/fabricmc/fabric/api/datagen/v1/FabricDataGenerator";
        let mut files = Vec::new();
        let mut report = Report::default();
        let written = emit_role_shim(
            &mut files,
            &mut report,
            &root,
            &none,
            &BTreeMap::new(),
            generator,
            &["mod_container", "identifier"],
            || FABRIC_DATA_GENERATOR.to_owned(),
        )
        .expect("emit a datagen shim without its roles");
        assert!(!written && files.is_empty());
        assert_eq!(
            report.missing,
            vec![format!("{generator} (needs mod_container, identifier)")]
        );
        let real = BTreeSet::from([generator.to_owned()]);
        let mut report = Report::default();
        let written = emit_role_shim(
            &mut files,
            &mut report,
            &root,
            &real,
            &BTreeMap::new(),
            generator,
            &[],
            || FABRIC_DATA_GENERATOR.to_owned(),
        )
        .expect("emit over a real datagen API class");
        assert!(!written && files.is_empty() && report.missing.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    /// The datagen generator is compiled and run for real: the pack keeps each
    /// provider's id and kind in call order and hands the same output back.
    /// Skipped cleanly when no JDK is installed.
    #[test]
    fn datagen_facade_compiles_and_retains_providers() {
        let types = DATAGEN_TYPE_CANDIDATES
            .iter()
            .map(|(role, names)| (*role, names[0].to_owned()))
            .collect::<BTreeMap<_, _>>();
        let probe = r#"import java.nio.file.Paths;
import java.util.List;
import net.fabricmc.loader.api.ModContainer;
import net.fabricmc.fabric.api.datagen.v1.*;
import net.fabricmc.fabric.api.datagen.v1.provider.*;
public class DatagenProbe {
    static class Recipes extends FabricRecipeProvider {
        Recipes(FabricDataOutput output) { super(output, null); }
    }
    static class Lang extends FabricLanguageProvider {
        Lang(FabricDataOutput output, String modId) { super(output, modId); }
        @Override public String getName() { return "en_us"; }
    }
    public static void main(String[] args) {
        ModContainer mod = ModContainer.horizon("demo");
        FabricDataOutput output = new FabricDataOutput(mod, Paths.get("/work/out"), false);
        FabricDataGenerator generator = new FabricDataGenerator(output, mod);
        DataGeneratorEntrypoint entry = g -> {
            FabricDataGenerator.Pack pack = g.createPack();
            pack.addProvider(new Recipes(g.getOutput()));
            pack.addProvider(o -> new Lang(o, "demo"));
        };
        entry.onInitializeDataGenerator(generator);
        List<Object[]> rows = generator.horizonProviders();
        if (rows.size() != 2) throw new AssertionError("rows " + rows.size());
        if (!"demo".equals(rows.get(0)[0]) || !"Recipes".equals(rows.get(0)[1]) || !"recipe".equals(rows.get(0)[2])) throw new AssertionError("recipe row");
        if (!"en_us".equals(rows.get(1)[1]) || !"language".equals(rows.get(1)[2])) throw new AssertionError("language row");
        if (!(rows.get(0)[3] instanceof Recipes) || !(rows.get(1)[3] instanceof Lang)) throw new AssertionError("provider kept");
        FabricDataGenerator.Pack pack = generator.createPack();
        pack.addProvider((DataProvider) null);
        pack.addProvider((DataProvider.Factory) null);
        if (generator.horizonProviders().size() != 2) throw new AssertionError("nulls retained");
        if (generator.getOutput() != output) throw new AssertionError("output");
        if (!"demo".equals(generator.getModId()) || !"demo".equals(output.getModId())) throw new AssertionError("mod id");
        if (output.getOutputFolder() == null) throw new AssertionError("folder");
        if (!"resourcepacks/demo:pack".equals(generator.createBuiltinResourcePack(new net.minecraft.resources.Identifier("demo", "pack")).getPackId())) throw new AssertionError("builtin pack");
        System.out.println("horizon datagen facade ok");
    }
}
"#;
        let root =
            std::env::temp_dir().join(format!("horizon-facade-datagen-run-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        let mut files = Vec::new();
        let mut write = |relative: &str, text: &str| {
            let file = src.join(relative);
            fs::create_dir_all(file.parent().expect("package directory")).expect("create package");
            fs::write(&file, text).expect("write java shim");
            files.push(file);
        };
        for (class, source, _) in FABRIC_DATAGEN_SHIMS {
            write(&format!("{class}.java"), &fill_types(source, &types));
        }
        write(
            "net/fabricmc/loader/api/ModContainer.java",
            "package net.fabricmc.loader.api; public class ModContainer { private final String id; public ModContainer(String id) { this.id = id; } public static ModContainer horizon(String id) { return new ModContainer(id); } public Metadata getMetadata() { return new Metadata(id); } public static class Metadata { private final String id; Metadata(String id) { this.id = id; } public String getId() { return id; } public String getName() { return id; } } }",
        );
        write(
            "net/minecraft/resources/Identifier.java",
            "package net.minecraft.resources; public class Identifier { private final String namespace; private final String path; public Identifier(String namespace, String path) { this.namespace = namespace; this.path = path; } public String toString() { return namespace + \":\" + path; } }",
        );
        write("DatagenProbe.java", probe);
        fs::create_dir_all(&classes).expect("create classes directory");
        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let tool = |name: &str| {
            jdk_bin
                .as_ref()
                .map(|bin| bin.join(name))
                .unwrap_or_else(|| PathBuf::from(name))
        };
        let Ok(javac_status) = Command::new(tool("javac"))
            .arg("-nowarn")
            .arg("-d")
            .arg(&classes)
            .args(&files)
            .status()
        else {
            let _ = fs::remove_dir_all(&root);
            return;
        };
        assert!(
            javac_status.success(),
            "javac rejected the generated datagen façade"
        );
        let output = Command::new(tool("java"))
            .arg("-cp")
            .arg(&classes)
            .arg("DatagenProbe")
            .output()
            .expect("run the datagen probe");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "datagen probe failed: {stderr}");
        assert!(
            stdout.contains("horizon datagen facade ok"),
            "probe output: {stdout}{stderr}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn mob_health_attribute_and_navigation_facades_keep_bounded_values() {
        for (method, ret, descriptor, expected) in [
            ("getHealth", "float", "()F", "return horizonHealth;"),
            ("getMaxHealth", "float", "()F", "return horizonMaxHealth;"),
            ("setHealth", "void", "(F)V", "horizonHealth = p0;"),
            (
                "getAttributes",
                "net.minecraft.world.entity.ai.attributes.AttributeMap",
                "()Lnet/minecraft/world/entity/ai/attributes/AttributeMap;",
                "return horizonAttributeMap;",
            ),
            (
                "getAttribute",
                "net.minecraft.world.entity.ai.attributes.AttributeInstance",
                "(Lnet/minecraft/core/Holder;)Lnet/minecraft/world/entity/ai/attributes/AttributeInstance;",
                "return horizonAttributes.get(p0);",
            ),
            (
                "getAttributeValue",
                "double",
                "(Lnet/minecraft/core/Holder;)D",
                "return 0.0;",
            ),
            (
                "getNavigation",
                "net.minecraft.world.entity.ai.navigation.PathNavigation",
                "()Lnet/minecraft/world/entity/ai/navigation/PathNavigation;",
                "return horizonNavigation;",
            ),
            (
                "finalizeSpawn",
                "net.minecraft.world.entity.SpawnGroupData",
                "(Lnet/minecraft/world/level/ServerLevelAccessor;Lnet/minecraft/world/DifficultyInstance;Lnet/minecraft/world/entity/EntitySpawnReason;Lnet/minecraft/world/entity/SpawnGroupData;)Lnet/minecraft/world/entity/SpawnGroupData;",
                "horizonSpawnGroupData = p3; return p3;",
            ),
            (
                "convertTo",
                "net.minecraft.world.entity.Mob",
                "(Lnet/minecraft/world/entity/EntityType;Lnet/minecraft/world/entity/ConversionParams;Lnet/minecraft/world/entity/EntitySpawnReason;Lnet/minecraft/world/entity/ConversionParams$AfterConversion;)Lnet/minecraft/world/entity/Mob;",
                "return null;",
            ),
        ] {
            assert_eq!(
                runtime_body_with_descriptor(
                    "net/minecraft/world/entity/Mob",
                    method,
                    ret,
                    descriptor
                ),
                expected,
                "Mob.{method}"
            );
        }
        // 26.3 declares the attribute accessors on `LivingEntity`; a mod whose
        // receiver is typed there emits that owner instead of `Mob`.
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/LivingEntity",
                "getAttributes",
                "net.minecraft.world.entity.ai.attributes.AttributeMap",
                "()Lnet/minecraft/world/entity/ai/attributes/AttributeMap;",
            ),
            "return horizonAttributeMap;"
        );
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/world/entity/LivingEntity",
                "getHealth",
                "float",
                "()F",
            ),
            "return horizonHealth;"
        );
        // `SpawnGroupData` itself is the empty 26.3 marker interface.
        let mut report = Report::default();
        let source = render(
            &Class {
                name: "net/minecraft/world/entity/SpawnGroupData".to_owned(),
                access: 0x0201,
                superclass: "java/lang/Object".to_owned(),
                ..Class::default()
            },
            None,
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut report,
            false,
            &[],
        );
        assert!(source.contains("public interface SpawnGroupData"));

        let mob = render(
            &Class {
                name: "net/minecraft/world/entity/Mob".to_owned(),
                superclass: "net/minecraft/world/entity/LivingEntity".to_owned(),
                ..Class::default()
            },
            None,
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut Report::default(),
            false,
            &[],
        );
        for field in [
            "horizonAttributes",
            "horizonAttributeMap",
            "horizonNavigation",
            "horizonGoalSelector",
            "horizonTargetSelector",
            "horizonSpawnGroupData",
        ] {
            assert!(mob.contains(field), "Mob is missing {field}");
        }
    }

    #[test]
    fn attribute_facades_match_the_26_3_member_names() {
        for (method, ret, expected) in [
            ("getDefaultValue", "double", "return horizonBaseValue;"),
            ("isClientSyncable", "boolean", "return horizonSyncable;"),
            (
                "setSyncable",
                "net.minecraft.world.entity.ai.attributes.Attribute",
                "horizonSyncable = p0; return this;",
            ),
            (
                "setSentiment",
                "net.minecraft.world.entity.ai.attributes.Attribute",
                "horizonSentiment = p0; return this;",
            ),
            ("sanitizeValue", "double", "return p0;"),
        ] {
            assert_eq!(
                runtime_body(
                    "net/minecraft/world/entity/ai/attributes/Attribute",
                    method,
                    ret
                ),
                expected,
                "Attribute.{method}"
            );
        }
        // `getBaseValue` is the Yarn spelling; 26.3's own name is covered above.
        assert_eq!(
            runtime_body(
                "net/minecraft/world/entity/ai/attributes/Attribute",
                "getBaseValue",
                "double"
            ),
            "return horizonBaseValue;"
        );
        for (method, expected) in [
            ("getMinValue", "return horizonMinValue;"),
            ("getMaxValue", "return horizonMaxValue;"),
            ("getDefaultValue", "return horizonBaseValue;"),
        ] {
            assert_eq!(
                runtime_body(
                    "net/minecraft/world/entity/ai/attributes/RangedAttribute",
                    method,
                    "double"
                ),
                expected,
                "RangedAttribute.{method}"
            );
        }
        assert_eq!(
            runtime_body(
                "net/minecraft/world/entity/ai/attributes/AttributeSupplier",
                "getBaseValue",
                "double"
            ),
            "return 0.0;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/entity/ai/attributes/AttributeSupplier",
                "hasModifier",
                "boolean"
            ),
            "return horizonAttributes.containsKey(p0);"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/entity/ai/attributes/AttributeSupplier",
                "builder",
                "net.minecraft.world.entity.ai.attributes.AttributeSupplier.Builder"
            ),
            "return new net.minecraft.world.entity.ai.attributes.AttributeSupplier.Builder();"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/entity/ai/attributes/AttributeSupplier$Builder",
                "add",
                "net.minecraft.world.entity.ai.attributes.AttributeSupplier.Builder"
            ),
            "return this;"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/entity/ai/attributes/AttributeSupplier$Builder",
                "build",
                "net.minecraft.world.entity.ai.attributes.AttributeSupplier"
            ),
            "return new net.minecraft.world.entity.ai.attributes.AttributeSupplier();"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/entity/ai/attributes/AttributeInstance",
                "setBaseValue",
                "void"
            ),
            "horizonBaseValue = p0; horizonValue = p0;"
        );
        let source = render(
            &Class {
                name: "net/minecraft/world/entity/ai/attributes/Attribute".to_owned(),
                superclass: "java/lang/Object".to_owned(),
                ..Class::default()
            },
            None,
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut Report::default(),
            false,
            &[],
        );
        assert!(source.contains("horizonSyncable"));
        assert!(source.contains("horizonSentiment"));
    }

    #[test]
    fn synched_entity_data_and_damage_records_keep_bounded_values() {
        assert_eq!(
            runtime_body_with_descriptor(
                "net/minecraft/network/syncher/SynchedEntityData",
                "defineId",
                "net.minecraft.network.syncher.EntityDataAccessor",
                "(Ljava/lang/Class;Lnet/minecraft/network/syncher/EntityDataSerializer;)Lnet/minecraft/network/syncher/EntityDataAccessor;",
            ),
            "return new net.minecraft.network.syncher.EntityDataAccessor(horizonNextId++, p1);"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/network/syncher/SynchedEntityData",
                "set",
                "void"
            ),
            "horizonItems.put(p0, p1); horizonDirty.add(p0);"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/network/syncher/SynchedEntityData",
                "isDirty",
                "boolean"
            ),
            "return !horizonDirty.isEmpty();"
        );
        assert!(
            runtime_body(
                "net/minecraft/network/syncher/SynchedEntityData",
                "packDirty",
                "java.util.List"
            )
            .contains("horizonDirty.clear()")
        );
        assert!(
            runtime_body(
                "net/minecraft/network/syncher/SynchedEntityData",
                "assignValues",
                "void"
            )
            .contains("horizonAssigned.addAll")
        );
        // A record's component accessor is generated by the record header, so
        // the Yarn `getId` drift edge delegates to it rather than to a field.
        assert_eq!(
            runtime_body(
                "net/minecraft/network/syncher/EntityDataAccessor",
                "getId",
                "int"
            ),
            "return id();"
        );
        assert_eq!(
            runtime_body(
                "net/minecraft/world/damagesource/DamageSource",
                "getEntity",
                "net.minecraft.world.entity.Entity"
            ),
            "return horizonEntity;"
        );
        for (method, ret, expected) in [
            ("isDirect", "boolean", "return horizonDirectEntity != null;"),
            (
                "getDirectEntity",
                "net.minecraft.world.entity.Entity",
                "return horizonDirectEntity;",
            ),
            (
                "getSourcePosition",
                "net.minecraft.world.phys.Vec3",
                "return horizonSourcePosition;",
            ),
            (
                "getMsgId",
                "java.lang.String",
                "return horizonType == null ? \"generic\" : horizonType.msgId();",
            ),
            (
                "getFoodExhaustion",
                "float",
                "return horizonType == null ? 0.0f : horizonType.exhaustion();",
            ),
        ] {
            assert_eq!(
                runtime_body("net/minecraft/world/damagesource/DamageSource", method, ret),
                expected,
                "DamageSource.{method}"
            );
        }
        let source = render(
            &Class {
                name: "net/minecraft/network/syncher/SynchedEntityData".to_owned(),
                ..Class::default()
            },
            None,
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut Report::default(),
            false,
            &[],
        );
        assert!(source.contains("horizonNextId"));
        assert!(source.contains("horizonItems"));
        assert!(source.contains("horizonDirty"));
    }

    #[test]
    fn record_facades_do_not_declare_instance_fields() {
        // javac rejects an instance field on a record with "field declaration
        // must be static"; `DamageType` and `EntityDataAccessor` are records in
        // 26.3, so only their components carry state.
        let damage_type = Class {
            name: "net/minecraft/world/damagesource/DamageType".to_owned(),
            superclass: "java/lang/Record".to_owned(),
            declared: vec![
                Declared {
                    name: "msgId".to_owned(),
                    descriptor: "Ljava/lang/String;".to_owned(),
                    access: 0x0012,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "exhaustion".to_owned(),
                    descriptor: "F".to_owned(),
                    access: 0x0012,
                    kind: MemberKind::Field,
                },
            ],
            ..Class::default()
        };
        let accessor = Class {
            name: "net/minecraft/network/syncher/EntityDataAccessor".to_owned(),
            superclass: "java/lang/Record".to_owned(),
            declared: vec![Declared {
                name: "id".to_owned(),
                descriptor: "I".to_owned(),
                access: 0x0012,
                kind: MemberKind::Field,
            }],
            ..Class::default()
        };
        // `CustomPacketPayload$Type` is a one-component record in 26.3. The
        // `id()` accessor comes from the component, so a `horizonId` field
        // would fail javac exactly like the two above.
        let payload_type = Class {
            name: "net/minecraft/network/protocol/common/custom/CustomPacketPayload$Type"
                .to_owned(),
            superclass: "java/lang/Record".to_owned(),
            declared: vec![
                Declared {
                    name: "id".to_owned(),
                    descriptor: "Lnet/minecraft/resources/Identifier;".to_owned(),
                    access: 0x0012,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "id".to_owned(),
                    descriptor: "()Lnet/minecraft/resources/Identifier;".to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        for (class, forbidden) in [
            (&damage_type, "horizonMsgId"),
            (&accessor, "horizonId"),
            (&payload_type, "horizonId"),
        ] {
            let source = render(
                class,
                None,
                &BTreeMap::new(),
                &BTreeSet::new(),
                None,
                &mut Report::default(),
                false,
                &[],
            );
            assert!(source.contains("record "), "{} is not a record", class.name);
            assert!(
                !source.contains(forbidden),
                "{} declares {forbidden}",
                class.name
            );
        }
        let payload_source = render(
            &payload_type,
            None,
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut Report::default(),
            false,
            &[],
        );
        assert!(
            payload_source
                .contains("record CustomPacketPayload$Type(net.minecraft.resources.Identifier id)"),
            "{payload_source}"
        );
        assert!(
            !payload_source.contains("public net.minecraft.resources.Identifier id()"),
            "the record already supplies id(): {payload_source}"
        );
        // Compile it: a record carrying a `horizonId` field or a second `id()`
        // fails javac, and this is the exact source the façade writes.
        use std::{path::PathBuf, process::Command};
        let root = std::env::temp_dir().join(format!(
            "horizon-facade-payload-type-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        let write = |relative: &str, text: &str| {
            let file = src.join(relative);
            fs::create_dir_all(file.parent().expect("package directory")).expect("create package");
            fs::write(&file, text).expect("write the java source");
            file
        };
        let files = [
            write(
                "net/minecraft/resources/Identifier.java",
                "package net.minecraft.resources;\npublic class Identifier {}\n",
            ),
            write(
                "net/minecraft/network/protocol/common/custom/CustomPacketPayload$Type.java",
                &payload_source,
            ),
        ];
        fs::create_dir_all(&classes).expect("create classes directory");
        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let javac = jdk_bin
            .as_ref()
            .map(|bin| bin.join("javac"))
            .unwrap_or_else(|| PathBuf::from("javac"));
        let Ok(status) = Command::new(&javac)
            .arg("-nowarn")
            .arg("-d")
            .arg(&classes)
            .args(&files)
            .status()
        else {
            let _ = fs::remove_dir_all(&root);
            return;
        };
        assert!(
            status.success(),
            "javac rejected the CustomPacketPayload$Type record"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn neoforge_entity_registration_shims_retain_markers() {
        assert_eq!(
            NEOFORGE_BIOME_MODIFIER,
            "net/neoforged/neoforge/common/world/BiomeModifier"
        );
        assert!(
            NEOFORGE_ENTITY_JOIN_LEVEL_EVENT_SOURCE.contains("extends net.neoforged.bus.api.Event")
        );
        assert!(NEOFORGE_ENTITY_JOIN_LEVEL_EVENT_SOURCE.contains("getEntity"));
        assert!(NEOFORGE_ENTITY_JOIN_LEVEL_EVENT_SOURCE.contains("loadedFromDisk"));
        assert!(NEOFORGE_REGISTER_CAPABILITIES_EVENT_SOURCE.contains("registerBlockEntity"));
        assert!(NEOFORGE_REGISTER_CAPABILITIES_EVENT_SOURCE.contains("registerBlock"));
        assert!(NEOFORGE_REGISTER_CAPABILITIES_EVENT_SOURCE.contains("registerItem"));
        assert!(NEOFORGE_REGISTER_CAPABILITIES_EVENT_SOURCE.contains("registerEntity"));
        assert!(NEOFORGE_REGISTER_CAPABILITIES_EVENT_SOURCE.contains("horizonKeep"));
        assert!(NEOFORGE_REGISTER_CAPABILITIES_EVENT_SOURCE.contains("isBlockRegistered"));
        assert!(NEOFORGE_BIOME_MODIFIER_SOURCE.contains("enum Phase"));
        assert!(NEOFORGE_BIOME_MODIFIER_SOURCE.contains("codec"));
        assert!(NEOFORGE_ISTORAGE_SOURCE.contains("readNBT"));
        assert!(NEOFORGE_ISTORAGE_SOURCE.contains("writeNBT"));
        // `IStorage` is a legacy marker (no 26.3 class) whose real descriptor
        // names `Capability<T>`/`Direction`/`CompoundTag`; its three Object
        // parameters are the bound, not an oversight.
        assert!(
            NEOFORGE_ISTORAGE_SOURCE
                .contains("default Object readNBT(Object capability, T instance, Object side)")
        );
        assert!(NEOFORGE_ISTORAGE_SOURCE.contains(
            "default void writeNBT(Object capability, T instance, Object side, Object nbt)"
        ));
        assert!(NEOFORGE_BIOME_MODIFIER_OBJECT_SOURCE.contains("modify(Object registryAccess"));
    }

    /// A bounded row is one per member, and the list cannot grow without a cap.
    #[test]
    fn legacy_bounds_are_deduplicated_and_capped() {
        let mut report = Report::default();
        report.bound_legacy("a/B", "m", "why");
        report.bound_legacy("a/B", "m", "why");
        assert_eq!(
            report.legacy,
            vec![LegacyBound {
                class: "a/B".to_owned(),
                member: "m".to_owned(),
                reason: "why".to_owned(),
            }]
        );
        for index in 0..MAX_LEGACY_BOUNDS + 8 {
            report.bound_legacy(format!("a/C{index}"), "m", "why");
        }
        assert_eq!(report.legacy.len(), MAX_LEGACY_BOUNDS);
    }

    /// The entity/attribute/synched-data/damage slice through the generator and
    /// javac: a miniature 26.3 game jar carries the real member shapes, a target
    /// class names them all, and the generated façade has to compile as one
    /// source set. That is the check a record field, a shadowed type or an
    /// undeclared support class fails before any mod can load.
    #[test]
    fn entity_attribute_and_damage_facades_generate_and_compile() {
        use crate::classes::tests_support::PoolBuilder;
        use std::{path::PathBuf, process::Command};

        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let tool = |name: &str| {
            jdk_bin
                .as_ref()
                .map(|bin| bin.join(name))
                .unwrap_or_else(|| PathBuf::from(name))
        };
        let Ok(version) = Command::new(tool("javac")).arg("-version").output() else {
            return;
        };
        if !version.status.success() {
            return;
        }

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("horizon-port-pack-entity-{stamp}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create the scratch root");
        let game_jar = root.join("game-26.3.jar");
        {
            let file = fs::File::create(&game_jar).expect("create the game jar");
            let mut jar = zip::ZipWriter::new(file);
            let mut class = |name: &str, superclass: &str, members: &[(&str, &str, u16)]| {
                let mut pool = PoolBuilder::default();
                let bytes = pool.build_with_members(name, superclass, members, &[]);
                jar.start_file(
                    format!("{name}.class"),
                    zip::write::SimpleFileOptions::default(),
                )
                .expect("start the game class");
                std::io::Write::write_all(&mut jar, &bytes).expect("write the game class");
            };
            let attr = "net/minecraft/world/entity/ai/attributes/Attribute";
            let instance = "net/minecraft/world/entity/ai/attributes/AttributeInstance";
            let supplier = "net/minecraft/world/entity/ai/attributes/AttributeSupplier";
            let builder = "net/minecraft/world/entity/ai/attributes/AttributeSupplier$Builder";
            let accessor = "net/minecraft/network/syncher/EntityDataAccessor";
            let mob = "net/minecraft/world/entity/Mob";
            let living = "net/minecraft/world/entity/LivingEntity";
            class(
                "net/minecraft/world/entity/SpawnGroupData",
                "java/lang/Object",
                &[],
            );
            class("net/minecraft/core/Holder", "java/lang/Object", &[]);
            class(
                "net/minecraft/world/item/ItemStack",
                "java/lang/Object",
                &[
                    ("<init>", "(Lnet/minecraft/world/item/Item;)V", 0x0001),
                    ("<init>", "(Lnet/minecraft/world/item/Item;I)V", 0x0001),
                ],
            );
            class("net/minecraft/world/item/Item", "java/lang/Object", &[]);
            class(
                "net/minecraft/core/component/DataComponentType",
                "java/lang/Object",
                &[],
            );
            class(
                "net/minecraft/world/effect/MobEffect",
                "java/lang/Object",
                &[],
            );
            class(
                "net/minecraft/world/effect/MobEffectInstance",
                "java/lang/Object",
                &[],
            );
            class(
                "net/minecraft/world/entity/ai/attributes/AttributeMap",
                "java/lang/Object",
                &[],
            );
            class(
                "net/minecraft/world/entity/ai/navigation/PathNavigation",
                "java/lang/Object",
                &[],
            );
            class(
                "net/minecraft/world/entity/ai/goal/GoalSelector",
                "java/lang/Object",
                &[],
            );
            class(
                "net/minecraft/network/syncher/EntityDataSerializer",
                "java/lang/Object",
                &[],
            );
            class(
                living,
                "java/lang/Object",
                &[
                    ("getHealth", "()F", 0x0001),
                    ("getMaxHealth", "()F", 0x0001),
                    ("setHealth", "(F)V", 0x0001),
                    ("heal", "(F)V", 0x0001),
                    (
                        "getAttribute",
                        "(Lnet/minecraft/core/Holder;)Lnet/minecraft/world/entity/ai/attributes/AttributeInstance;",
                        0x0001,
                    ),
                    (
                        "getAttributes",
                        "()Lnet/minecraft/world/entity/ai/attributes/AttributeMap;",
                        0x0001,
                    ),
                ],
            );
            class(
                mob,
                living,
                &[
                    ("isNoAi", "()Z", 0x0001),
                    ("setNoAi", "(Z)V", 0x0001),
                    (
                        "getAttribute",
                        "(Lnet/minecraft/core/Holder;)Lnet/minecraft/world/entity/ai/attributes/AttributeInstance;",
                        0x0001,
                    ),
                    (
                        "getAttributes",
                        "()Lnet/minecraft/world/entity/ai/attributes/AttributeMap;",
                        0x0001,
                    ),
                    (
                        "getNavigation",
                        "()Lnet/minecraft/world/entity/ai/navigation/PathNavigation;",
                        0x0001,
                    ),
                    (
                        "finalizeSpawn",
                        "(Lnet/minecraft/world/level/ServerLevelAccessor;Lnet/minecraft/world/DifficultyInstance;Lnet/minecraft/world/entity/EntitySpawnReason;Lnet/minecraft/world/entity/SpawnGroupData;)Lnet/minecraft/world/entity/SpawnGroupData;",
                        0x0001,
                    ),
                ],
            );
            class(
                attr,
                "java/lang/Object",
                &[
                    ("getDefaultValue", "()D", 0x0001),
                    ("getDescriptionId", "()Ljava/lang/String;", 0x0001),
                    ("isClientSyncable", "()Z", 0x0001),
                    (
                        "setSyncable",
                        "(Z)Lnet/minecraft/world/entity/ai/attributes/Attribute;",
                        0x0001,
                    ),
                ],
            );
            class(
                "net/minecraft/world/entity/ai/attributes/RangedAttribute",
                attr,
                &[
                    ("getMinValue", "()D", 0x0001),
                    ("getMaxValue", "()D", 0x0001),
                ],
            );
            class(
                instance,
                "java/lang/Object",
                &[
                    ("getBaseValue", "()D", 0x0001),
                    ("getValue", "()D", 0x0001),
                    ("setBaseValue", "(D)V", 0x0001),
                ],
            );
            class(
                supplier,
                "java/lang/Object",
                &[
                    ("getBaseValue", "(Lnet/minecraft/core/Holder;)D", 0x0001),
                    ("getValue", "(Lnet/minecraft/core/Holder;)D", 0x0001),
                    (
                        "createInstance",
                        "(Ljava/util/function/Consumer;Lnet/minecraft/core/Holder;)Lnet/minecraft/world/entity/ai/attributes/AttributeInstance;",
                        0x0001,
                    ),
                    (
                        "builder",
                        "()Lnet/minecraft/world/entity/ai/attributes/AttributeSupplier$Builder;",
                        0x0009,
                    ),
                    ("hasAttribute", "(Lnet/minecraft/core/Holder;)Z", 0x0001),
                ],
            );
            class(
                builder,
                "java/lang/Object",
                &[
                    (
                        "add",
                        "(Lnet/minecraft/core/Holder;)Lnet/minecraft/world/entity/ai/attributes/AttributeSupplier$Builder;",
                        0x0001,
                    ),
                    (
                        "build",
                        "()Lnet/minecraft/world/entity/ai/attributes/AttributeSupplier;",
                        0x0001,
                    ),
                ],
            );
            class(
                "net/minecraft/network/syncher/SynchedEntityData",
                "java/lang/Object",
                &[
                    (
                        "defineId",
                        "(Ljava/lang/Class;Lnet/minecraft/network/syncher/EntityDataSerializer;)Lnet/minecraft/network/syncher/EntityDataAccessor;",
                        0x0009,
                    ),
                    (
                        "get",
                        "(Lnet/minecraft/network/syncher/EntityDataAccessor;)Ljava/lang/Object;",
                        0x0001,
                    ),
                    (
                        "set",
                        "(Lnet/minecraft/network/syncher/EntityDataAccessor;Ljava/lang/Object;)V",
                        0x0001,
                    ),
                    ("isDirty", "()Z", 0x0001),
                    ("packDirty", "()Ljava/util/List;", 0x0001),
                    ("assignValues", "(Ljava/util/List;)V", 0x0001),
                ],
            );
            class(
                accessor,
                "java/lang/Object",
                &[
                    (
                        "<init>",
                        "(ILnet/minecraft/network/syncher/EntityDataSerializer;)V",
                        0x0001,
                    ),
                    ("id", "()I", 0x0001),
                ],
            );
            jar.finish().expect("finish the game jar");
        }
        // The target's constant pool names every member the slice implements;
        // the class body itself only returns.
        let target = root.join("demo-mod.jar");
        {
            let file = fs::File::create(&target).expect("create the target jar");
            let mut jar = zip::ZipWriter::new(file);
            let mut pool = PoolBuilder::default();
            for (owner, name, descriptor) in [
                ("net/minecraft/world/entity/Mob", "isNoAi", "()Z"),
                ("net/minecraft/world/entity/Mob", "getHealth", "()F"),
                ("net/minecraft/world/entity/Mob", "setHealth", "(F)V"),
                (
                    "net/minecraft/world/entity/Mob",
                    "getAttribute",
                    "(Lnet/minecraft/core/Holder;)Lnet/minecraft/world/entity/ai/attributes/AttributeInstance;",
                ),
                (
                    "net/minecraft/world/entity/Mob",
                    "getAttributes",
                    "()Lnet/minecraft/world/entity/ai/attributes/AttributeMap;",
                ),
                (
                    "net/minecraft/world/entity/Mob",
                    "getNavigation",
                    "()Lnet/minecraft/world/entity/ai/navigation/PathNavigation;",
                ),
                (
                    "net/minecraft/world/entity/Mob",
                    "finalizeSpawn",
                    "(Lnet/minecraft/world/level/ServerLevelAccessor;Lnet/minecraft/world/DifficultyInstance;Lnet/minecraft/world/entity/EntitySpawnReason;Lnet/minecraft/world/entity/SpawnGroupData;)Lnet/minecraft/world/entity/SpawnGroupData;",
                ),
                (
                    "net/minecraft/world/entity/LivingEntity",
                    "getAttributes",
                    "()Lnet/minecraft/world/entity/ai/attributes/AttributeMap;",
                ),
                (
                    "net/minecraft/world/entity/ai/attributes/Attribute",
                    "getDefaultValue",
                    "()D",
                ),
                (
                    "net/minecraft/world/entity/ai/attributes/Attribute",
                    "setSyncable",
                    "(Z)Lnet/minecraft/world/entity/ai/attributes/Attribute;",
                ),
                (
                    "net/minecraft/world/entity/ai/attributes/RangedAttribute",
                    "getMinValue",
                    "()D",
                ),
                (
                    "net/minecraft/world/entity/ai/attributes/AttributeSupplier",
                    "getBaseValue",
                    "(Lnet/minecraft/core/Holder;)D",
                ),
                (
                    "net/minecraft/world/entity/ai/attributes/AttributeSupplier",
                    "createInstance",
                    "(Ljava/util/function/Consumer;Lnet/minecraft/core/Holder;)Lnet/minecraft/world/entity/ai/attributes/AttributeInstance;",
                ),
                (
                    "net/minecraft/world/entity/ai/attributes/AttributeSupplier",
                    "builder",
                    "()Lnet/minecraft/world/entity/ai/attributes/AttributeSupplier$Builder;",
                ),
                (
                    "net/minecraft/world/entity/ai/attributes/AttributeSupplier$Builder",
                    "add",
                    "(Lnet/minecraft/core/Holder;)Lnet/minecraft/world/entity/ai/attributes/AttributeSupplier$Builder;",
                ),
                (
                    "net/minecraft/world/entity/ai/attributes/AttributeSupplier$Builder",
                    "build",
                    "()Lnet/minecraft/world/entity/ai/attributes/AttributeSupplier;",
                ),
                (
                    "net/minecraft/world/entity/ai/attributes/AttributeInstance",
                    "getBaseValue",
                    "()D",
                ),
                (
                    "net/minecraft/network/syncher/SynchedEntityData",
                    "defineId",
                    "(Ljava/lang/Class;Lnet/minecraft/network/syncher/EntityDataSerializer;)Lnet/minecraft/network/syncher/EntityDataAccessor;",
                ),
                (
                    "net/minecraft/network/syncher/SynchedEntityData",
                    "get",
                    "(Lnet/minecraft/network/syncher/EntityDataAccessor;)Ljava/lang/Object;",
                ),
                (
                    "net/minecraft/network/syncher/SynchedEntityData",
                    "set",
                    "(Lnet/minecraft/network/syncher/EntityDataAccessor;Ljava/lang/Object;)V",
                ),
                (
                    "net/minecraft/network/syncher/SynchedEntityData",
                    "packDirty",
                    "()Ljava/util/List;",
                ),
                (
                    "net/minecraft/network/syncher/SynchedEntityData",
                    "assignValues",
                    "(Ljava/util/List;)V",
                ),
                (
                    "net/minecraft/network/syncher/EntityDataAccessor",
                    "id",
                    "()I",
                ),
            ] {
                pool.method(owner, name, descriptor);
            }
            let bytes = pool.build("demo/Probe", "java/lang/Object", &[("run", &[0xb1])]);
            jar.start_file("demo/Probe.class", zip::write::SimpleFileOptions::default())
                .expect("start the target class");
            std::io::Write::write_all(&mut jar, &bytes).expect("write the target class");
            jar.finish().expect("finish the target jar");
        }
        let out = root.join("facade");
        let report = generate(&target, &game_jar, &[], &out, &tool("javac"), &tool("jar"))
            .unwrap_or_else(|e| {
                panic!(
                    "the façade did not generate: {e} (sources in {})",
                    out.display()
                )
            });
        assert!(
            report.jar.is_file(),
            "the façade jar was not written: {report:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// An entity-only or block-entity-only target is what the support list is
    /// for: the generated value carries fields and accessors naming `Level`,
    /// `EntityType`, `AABB`, `BlockState` and `CompoundTag`, none of which the
    /// mod mentioned itself. Generate both façades from a miniature game jar
    /// and let javac judge the slice.
    #[test]
    fn entity_and_block_entity_only_facades_compile() {
        use crate::classes::tests_support::PoolBuilder;
        use std::{path::PathBuf, process::Command};

        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let tool = |name: &str| {
            jdk_bin
                .as_ref()
                .map(|bin| bin.join(name))
                .unwrap_or_else(|| PathBuf::from(name))
        };
        let Ok(version) = Command::new(tool("javac")).arg("-version").output() else {
            return;
        };
        if !version.status.success() {
            return;
        }

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("horizon-port-pack-entity-only-{stamp}"));
        let _ = fs::remove_dir_all(&root);
        let game_src = root.join("game-src");
        let game_classes = root.join("game-classes");
        let mut game_files = Vec::new();
        let mut write_game = |relative: &str, text: &str| {
            let file = game_src.join(relative);
            fs::create_dir_all(file.parent().expect("package directory"))
                .expect("create the game package");
            fs::write(&file, text).expect("write the game source");
            game_files.push(file);
        };
        // The real 26.3 shapes are richer; the point here is the type closure,
        // not the bodies. `MobCategory.MISC` and the six-argument box are the
        // two values the generated fields construct directly.
        for (relative, text) in [
            (
                "net/minecraft/world/entity/Entity.java",
                "package net.minecraft.world.entity;\npublic class Entity { public Entity() {} }\n",
            ),
            (
                "net/minecraft/world/entity/EntityType.java",
                "package net.minecraft.world.entity;\npublic class EntityType { private EntityType() {} }\n",
            ),
            (
                "net/minecraft/world/entity/EntityDimensions.java",
                "package net.minecraft.world.entity;\npublic class EntityDimensions { public EntityDimensions() {} }\n",
            ),
            (
                "net/minecraft/world/entity/MobCategory.java",
                "package net.minecraft.world.entity;\npublic enum MobCategory { MISC }\n",
            ),
            (
                "net/minecraft/world/phys/AABB.java",
                "package net.minecraft.world.phys;\npublic class AABB { public double minX; public double minY; public double minZ; public double maxX; public double maxY; public double maxZ; public AABB() {} public AABB(double a, double b, double c, double d, double e, double f) {} }\n",
            ),
            (
                "net/minecraft/world/phys/Vec3.java",
                "package net.minecraft.world.phys;\npublic class Vec3 { public Vec3() {} }\n",
            ),
            (
                "net/minecraft/world/level/Level.java",
                "package net.minecraft.world.level;\npublic class Level { public Level() {} }\n",
            ),
            (
                "net/minecraft/world/level/block/state/BlockState.java",
                "package net.minecraft.world.level.block.state;\npublic class BlockState { public BlockState() {} }\n",
            ),
            (
                "net/minecraft/world/level/block/Block.java",
                "package net.minecraft.world.level.block;\npublic class Block { public net.minecraft.world.level.block.state.StateDefinition stateDefinition; public Block() {} }\n",
            ),
            (
                "net/minecraft/world/level/block/state/StateDefinition.java",
                "package net.minecraft.world.level.block.state;\npublic class StateDefinition { public StateDefinition() {} }\n",
            ),
            (
                "net/minecraft/world/level/material/FluidState.java",
                "package net.minecraft.world.level.material;\npublic class FluidState { public FluidState() {} }\n",
            ),
            (
                "net/minecraft/world/level/block/entity/BlockEntity.java",
                "package net.minecraft.world.level.block.entity;\npublic class BlockEntity { public BlockEntity() {} }\n",
            ),
            (
                "net/minecraft/world/level/block/entity/BlockEntityType.java",
                "package net.minecraft.world.level.block.entity;\npublic class BlockEntityType { public BlockEntityType() {} }\n",
            ),
            (
                "net/minecraft/nbt/CompoundTag.java",
                "package net.minecraft.nbt;\npublic class CompoundTag { public CompoundTag() {} }\n",
            ),
            (
                "net/minecraft/core/BlockPos.java",
                "package net.minecraft.core;\npublic class BlockPos { public BlockPos() {} public int getX() { return 0; } public int getY() { return 0; } public int getZ() { return 0; } }\n",
            ),
            (
                "net/minecraft/resources/ResourceKey.java",
                "package net.minecraft.resources;\npublic class ResourceKey { private ResourceKey() {} }\n",
            ),
            (
                "net/minecraft/resources/Identifier.java",
                "package net.minecraft.resources;\npublic class Identifier { private Identifier() {} }\n",
            ),
        ] {
            write_game(relative, text);
        }
        fs::create_dir_all(&game_classes).expect("create the game classes directory");
        let status = Command::new(tool("javac"))
            .arg("-nowarn")
            .arg("-d")
            .arg(&game_classes)
            .args(&game_files)
            .status()
            .expect("run javac on the game shapes");
        assert!(
            status.success(),
            "the miniature game sources did not compile"
        );
        let game_jar = root.join("game-26.3.jar");
        let status = Command::new(tool("jar"))
            .arg("--create")
            .arg("--file")
            .arg(&game_jar)
            .arg("-C")
            .arg(&game_classes)
            .arg(".")
            .status()
            .expect("run jar on the game classes");
        assert!(status.success(), "the miniature game jar was not written");

        let target = |name: &str, owner: &str, method: &str, descriptor: &str| {
            let path = root.join(name);
            let file = fs::File::create(&path).expect("create the target jar");
            let mut jar = zip::ZipWriter::new(file);
            let mut pool = PoolBuilder::default();
            pool.method(owner, method, descriptor);
            let bytes = pool.build("demo/Probe", "java/lang/Object", &[("run", &[0xb1])]);
            jar.start_file("demo/Probe.class", zip::write::SimpleFileOptions::default())
                .expect("start the target class");
            std::io::Write::write_all(&mut jar, &bytes).expect("write the target class");
            jar.finish().expect("finish the target jar");
            path
        };
        for (name, owner, method, descriptor) in [
            (
                "entity-only.jar",
                "net/minecraft/world/entity/Entity",
                "getBlockX",
                "()I",
            ),
            (
                "block-entity-only.jar",
                "net/minecraft/world/level/block/entity/BlockEntity",
                "getBlockPos",
                "()Lnet/minecraft/core/BlockPos;",
            ),
        ] {
            let target = target(name, owner, method, descriptor);
            let out = root.join(name.trim_end_matches(".jar"));
            let report = generate(&target, &game_jar, &[], &out, &tool("javac"), &tool("jar"))
                .unwrap_or_else(|e| {
                    panic!(
                        "{owner}-only façade did not generate: {e} (sources in {})",
                        out.display()
                    )
                });
            assert!(
                report.jar.is_file(),
                "{owner}-only façade jar was not written: {report:?}"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// `DamageType` is a record and `DamageSource` is a plain value: render both
    /// and let javac decide. A hand-written record field or a method body that
    /// names a missing state member fails here.
    #[test]
    fn damage_type_and_source_facades_compile() {
        use std::{path::PathBuf, process::Command};
        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let javac = jdk_bin
            .as_ref()
            .map(|bin| bin.join("javac"))
            .unwrap_or_else(|| PathBuf::from("javac"));
        let Ok(version) = Command::new(&javac).arg("-version").output() else {
            return;
        };
        if !version.status.success() {
            return;
        }
        let damages = ["net/minecraft/world/damagesource/DamageType".to_owned()];
        let damage_type = Class {
            name: damages[0].clone(),
            superclass: "java/lang/Record".to_owned(),
            declared: vec![
                Declared {
                    name: "msgId".to_owned(),
                    descriptor: "Ljava/lang/String;".to_owned(),
                    access: 0x0012,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "scaling".to_owned(),
                    descriptor: "Lnet/minecraft/world/damagesource/DamageScaling;".to_owned(),
                    access: 0x0012,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "exhaustion".to_owned(),
                    descriptor: "F".to_owned(),
                    access: 0x0012,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "effects".to_owned(),
                    descriptor: "Lnet/minecraft/world/damagesource/DamageEffects;".to_owned(),
                    access: 0x0012,
                    kind: MemberKind::Field,
                },
                Declared {
                    name: "deathMessageType".to_owned(),
                    descriptor: "Lnet/minecraft/world/damagesource/DeathMessageType;".to_owned(),
                    access: 0x0012,
                    kind: MemberKind::Field,
                },
            ],
            ..Class::default()
        };
        let source = Class {
            name: "net/minecraft/world/damagesource/DamageSource".to_owned(),
            superclass: "java/lang/Object".to_owned(),
            declared: [
                ("type", "()Lnet/minecraft/world/damagesource/DamageType;"),
                ("getEntity", "()Lnet/minecraft/world/entity/Entity;"),
                ("getDirectEntity", "()Lnet/minecraft/world/entity/Entity;"),
                ("getSourcePosition", "()Lnet/minecraft/world/phys/Vec3;"),
                ("isDirect", "()Z"),
                ("getMsgId", "()Ljava/lang/String;"),
                ("getWeaponItem", "()Lnet/minecraft/world/item/ItemStack;"),
            ]
            .iter()
            .map(|(name, descriptor)| Declared {
                name: (*name).to_owned(),
                descriptor: (*descriptor).to_owned(),
                access: 0x0001,
                kind: MemberKind::Method,
            })
            .collect(),
            ..Class::default()
        };
        let used = source
            .declared
            .iter()
            .map(|member| (member.name.clone(), member.descriptor.clone()))
            .collect::<BTreeSet<_>>();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("horizon-port-pack-damage-{stamp}"));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        fs::create_dir_all(&src).expect("create the source root");
        fs::create_dir_all(&classes).expect("create the classes root");
        let mut files = Vec::new();
        let mut write = |path: &str, text: &str| {
            let file = src.join(path);
            fs::create_dir_all(file.parent().expect("package directory")).expect("create package");
            fs::write(&file, text).expect("write the source");
            files.push(file);
        };
        write(
            "net/minecraft/world/damagesource/DamageType.java",
            &render(
                &damage_type,
                None,
                &BTreeMap::new(),
                &BTreeSet::new(),
                None,
                &mut Report::default(),
                false,
                &[],
            ),
        );
        write(
            "net/minecraft/world/damagesource/DamageSource.java",
            &render(
                &source,
                Some(&used),
                &BTreeMap::new(),
                &BTreeSet::new(),
                None,
                &mut Report::default(),
                false,
                &[],
            ),
        );
        write(
            "net/minecraft/world/damagesource/DamageScaling.java",
            "package net.minecraft.world.damagesource;\npublic enum DamageScaling { NEVER, WHEN_CAUSED_BY_LIVING_NON_PLAYER, ALWAYS }\n",
        );
        write(
            "net/minecraft/world/damagesource/DamageEffects.java",
            "package net.minecraft.world.damagesource;\npublic enum DamageEffects { HURT, THORNS, DROWNING }\n",
        );
        write(
            "net/minecraft/world/damagesource/DeathMessageType.java",
            "package net.minecraft.world.damagesource;\npublic enum DeathMessageType { DEFAULT, FALL_VARIANTS, INTENTIONAL_GAME_DESIGN }\n",
        );
        write(
            "net/minecraft/world/entity/Entity.java",
            "package net.minecraft.world.entity;\npublic class Entity {}\n",
        );
        write(
            "net/minecraft/world/phys/Vec3.java",
            "package net.minecraft.world.phys;\npublic class Vec3 {}\n",
        );
        write(
            "net/minecraft/world/item/ItemStack.java",
            "package net.minecraft.world.item;\npublic class ItemStack {}\n",
        );
        let status = Command::new(&javac)
            .arg("-nowarn")
            .arg("-d")
            .arg(&classes)
            .args(&files)
            .status()
            .expect("run javac");
        assert!(status.success(), "javac rejected the damage façade");
        let _ = fs::remove_dir_all(&root);
    }

    /// Two generic overloads erase to one Java signature; the façade emits the
    /// first one only. Genuinely different erasures (the two- and three-argument
    /// `set`) both stay.
    #[test]
    fn facade_emits_one_method_per_erasure() {
        let class = Class {
            name: "net/minecraft/network/syncher/SynchedEntityData".to_owned(),
            superclass: "java/lang/Object".to_owned(),
            declared: vec![
                Declared {
                    name: "set".to_owned(),
                    descriptor:
                        "(Lnet/minecraft/network/syncher/EntityDataAccessor;Ljava/lang/Object;)V"
                            .to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "set".to_owned(),
                    descriptor:
                        "(Lnet/minecraft/network/syncher/EntityDataAccessor;Ljava/lang/Object;Z)V"
                            .to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "get".to_owned(),
                    descriptor:
                        "(Lnet/minecraft/network/syncher/EntityDataAccessor;)Ljava/lang/Object;"
                            .to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
                Declared {
                    name: "get".to_owned(),
                    descriptor:
                        "(Lnet/minecraft/network/syncher/EntityDataAccessor;)Ljava/lang/String;"
                            .to_owned(),
                    access: 0x0001,
                    kind: MemberKind::Method,
                },
            ],
            ..Class::default()
        };
        let used = class
            .declared
            .iter()
            .map(|member| (member.name.clone(), member.descriptor.clone()))
            .collect::<BTreeSet<_>>();
        let source = render(
            &class,
            Some(&used),
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &mut Report::default(),
            false,
            &[],
        );
        assert_eq!(
            source.matches("public void set(").count(),
            2,
            "both set erasures: {source}"
        );
        assert_eq!(
            source.matches("public java.lang.Object get(").count(),
            1,
            "one get erasure: {source}"
        );
        assert!(
            !source.contains("public java.lang.String get("),
            "the duplicate erasure was not deduped: {source}"
        );
    }

    /// The four NeoForge entity/capability/biome shims compile as a unit and
    /// expose the 26.3 member names a mod snapshots against.
    #[test]
    fn neoforge_entity_registration_shims_compile() {
        use std::{path::PathBuf, process::Command};
        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let javac = jdk_bin
            .as_ref()
            .map(|bin| bin.join("javac"))
            .unwrap_or_else(|| PathBuf::from("javac"));
        let Ok(version) = Command::new(&javac).arg("-version").output() else {
            return;
        };
        if !version.status.success() {
            return;
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("horizon-port-pack-neoforge-{stamp}"));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        fs::create_dir_all(&src).expect("create the source root");
        fs::create_dir_all(&classes).expect("create the classes root");
        let mut files = Vec::new();
        let mut write = |path: &str, text: &str| {
            let file = src.join(path);
            fs::create_dir_all(file.parent().expect("package directory")).expect("create package");
            fs::write(&file, text).expect("write the source");
            files.push(file);
        };
        write("net/neoforged/bus/api/Event.java", NEOFORGE_EVENT);
        // The 26.3 join-level event names the joined entity and its level.
        write(
            "net/minecraft/world/entity/Entity.java",
            "package net.minecraft.world.entity;\npublic class Entity {}\n",
        );
        write(
            "net/minecraft/world/level/Level.java",
            "package net.minecraft.world.level;\npublic class Level {}\n",
        );
        for (path, source) in [
            (
                "net/minecraft/core/RegistryAccess.java",
                "package net.minecraft.core; public class RegistryAccess {}\n",
            ),
            (
                "net/minecraft/core/Holder.java",
                "package net.minecraft.core; public class Holder<T> {}\n",
            ),
            (
                "net/minecraft/world/level/biome/Biome.java",
                "package net.minecraft.world.level.biome; public class Biome {}\n",
            ),
            (
                "net/minecraft/world/level/biome/BiomeGenerationSettings.java",
                "package net.minecraft.world.level.biome; public class BiomeGenerationSettings { public static class Builder {} }\n",
            ),
        ] {
            write(path, source);
        }
        for (path, source) in [
            (
                "net/minecraft/core/BlockPos.java",
                "package net.minecraft.core; public class BlockPos {}\n",
            ),
            (
                "net/minecraft/world/level/block/Block.java",
                "package net.minecraft.world.level.block; public class Block {}\n",
            ),
            (
                "net/minecraft/world/level/block/entity/BlockEntity.java",
                "package net.minecraft.world.level.block.entity; public class BlockEntity {}\n",
            ),
            (
                "net/minecraft/world/level/block/entity/BlockEntityType.java",
                "package net.minecraft.world.level.block.entity; public class BlockEntityType<T> {}\n",
            ),
            (
                "net/minecraft/world/level/block/state/BlockState.java",
                "package net.minecraft.world.level.block.state; public class BlockState {}\n",
            ),
            (
                "net/minecraft/world/entity/EntityType.java",
                "package net.minecraft.world.entity; public class EntityType<T> {}\n",
            ),
            (
                "net/minecraft/world/item/Item.java",
                "package net.minecraft.world.item; public class Item {}\n",
            ),
            (
                "net/minecraft/world/item/ItemStack.java",
                "package net.minecraft.world.item; public class ItemStack {}\n",
            ),
            (
                "net/neoforged/neoforge/capabilities/Capability.java",
                "package net.neoforged.neoforge.capabilities; public class Capability<T> {}\n",
            ),
            (
                "net/neoforged/neoforge/capabilities/BlockCapability.java",
                "package net.neoforged.neoforge.capabilities; public class BlockCapability<T,C> extends Capability<T> {}\n",
            ),
            (
                "net/neoforged/neoforge/capabilities/ItemCapability.java",
                "package net.neoforged.neoforge.capabilities; public class ItemCapability<T,C> extends Capability<T> {}\n",
            ),
            (
                "net/neoforged/neoforge/capabilities/EntityCapability.java",
                "package net.neoforged.neoforge.capabilities; public class EntityCapability<T,C> extends Capability<T> {}\n",
            ),
            (
                "net/neoforged/neoforge/capabilities/BlockCapabilityCache.java",
                "package net.neoforged.neoforge.capabilities; public class BlockCapabilityCache { @FunctionalInterface public interface Provider<T,C> { T getCapability(net.minecraft.world.level.Level l, net.minecraft.core.BlockPos p, net.minecraft.world.level.block.state.BlockState s, net.minecraft.world.level.block.entity.BlockEntity e, C c); } }\n",
            ),
            (
                "net/neoforged/neoforge/capabilities/ItemCapabilityCache.java",
                "package net.neoforged.neoforge.capabilities; public class ItemCapabilityCache { @FunctionalInterface public interface Provider<T,C> { T getCapability(net.minecraft.world.item.ItemStack s, C c); } }\n",
            ),
            (
                "net/neoforged/neoforge/capabilities/EntityCapabilityCache.java",
                "package net.neoforged.neoforge.capabilities; public class EntityCapabilityCache { @FunctionalInterface public interface Provider<T,C> { T getCapability(net.minecraft.world.entity.Entity e, C c); } }\n",
            ),
        ] {
            write(path, source);
        }
        for (class, source) in [
            (
                NEOFORGE_ENTITY_JOIN_LEVEL_EVENT,
                NEOFORGE_ENTITY_JOIN_LEVEL_EVENT_SOURCE,
            ),
            (
                NEOFORGE_REGISTER_CAPABILITIES_EVENT,
                NEOFORGE_REGISTER_CAPABILITIES_EVENT_SOURCE,
            ),
            (NEOFORGE_BIOME_MODIFIER, NEOFORGE_BIOME_MODIFIER_SOURCE),
            (NEOFORGE_ISTORAGE, NEOFORGE_ISTORAGE_SOURCE),
        ] {
            write(&format!("{class}.java"), source);
        }
        write(
            "Probe.java",
            r#"import net.neoforged.neoforge.event.entity.EntityJoinLevelEvent;
import net.neoforged.neoforge.capabilities.RegisterCapabilitiesEvent;
import net.neoforged.neoforge.common.world.BiomeModifier;
import net.neoforged.neoforge.common.capabilities.IStorage;
public class Probe {
    static void run(EntityJoinLevelEvent join, RegisterCapabilitiesEvent caps, BiomeModifier modifier, IStorage<Object> storage) {
        Object entity = join.getEntity();
        Object level = join.getLevel();
        boolean loaded = join.loadedFromDisk();
        join.setCanceled(true);
        caps.registerItem(null, null);
        caps.registerBlockEntity(null, null, null);
        caps.registerEntity(null, null, null);
        Object codec = modifier.codec();
        modifier.modify(null, null, BiomeModifier.Phase.ADD, null);
        Object read = storage.readNBT(null, null, null);
    }
}
"#,
        );
        let status = Command::new(&javac)
            .arg("-nowarn")
            .arg("-d")
            .arg(&classes)
            .args(&files)
            .status()
            .expect("run javac");
        assert!(status.success(), "javac rejected the NeoForge entity shims");
        let _ = fs::remove_dir_all(&root);
    }

    /// The whole NeoForge shim slice, not just the four entity events: a target
    /// that names the item-handler capability generates the loader, event,
    /// registry and capability files together, and `javac` compiles them as one
    /// source set. A shim missing its closing brace (or naming a game type the
    /// façade never emitted) fails here instead of in every NeoForge port.
    #[test]
    fn neoforge_facade_generates_and_compiles() {
        use crate::classes::tests_support::PoolBuilder;
        use std::{path::PathBuf, process::Command};

        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let tool = |name: &str| {
            jdk_bin
                .as_ref()
                .map(|bin| bin.join(name))
                .unwrap_or_else(|| PathBuf::from(name))
        };
        let Ok(version) = Command::new(tool("javac")).arg("-version").output() else {
            return;
        };
        if !version.status.success() {
            return;
        }

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("horizon-port-pack-neoforge-full-{stamp}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create the scratch root");

        // The game jar carries only the shapes the shim slice names: the item
        // handler reads `ItemStack`, `Item` and `Direction`, and the stack's
        // bounded value names its component type.
        let game_jar = root.join("game-26.3.jar");
        {
            let file = fs::File::create(&game_jar).expect("create the game jar");
            let mut jar = zip::ZipWriter::new(file);
            let mut class = |name: &str, superclass: &str, members: &[(&str, &str, u16)]| {
                let mut pool = PoolBuilder::default();
                let bytes = pool.build_with_members(name, superclass, members, &[]);
                jar.start_file(
                    format!("{name}.class"),
                    zip::write::SimpleFileOptions::default(),
                )
                .expect("start the game class");
                std::io::Write::write_all(&mut jar, &bytes).expect("write the game class");
            };
            class(
                "net/minecraft/world/item/ItemStack",
                "java/lang/Object",
                &[("<init>", "(Lnet/minecraft/world/item/Item;I)V", 0x0001)],
            );
            class("net/minecraft/world/item/Item", "java/lang/Object", &[]);
            class(
                "net/minecraft/core/component/DataComponentType",
                "java/lang/Object",
                &[],
            );
            class("net/minecraft/core/Direction", "java/lang/Object", &[]);
            class("net/minecraft/core/BlockPos", "java/lang/Object", &[]);
            class("net/minecraft/world/level/Level", "java/lang/Object", &[]);
            class(
                "net/minecraft/world/level/block/Block",
                "java/lang/Object",
                &[],
            );
            class(
                "net/minecraft/world/level/block/state/BlockState",
                "java/lang/Object",
                &[],
            );
            class(
                "net/minecraft/world/level/block/entity/BlockEntity",
                "java/lang/Object",
                &[],
            );
            class(
                "net/minecraft/world/level/block/entity/BlockEntityType",
                "java/lang/Object",
                &[],
            );
            class("net/minecraft/world/entity/Entity", "java/lang/Object", &[]);
            class(
                "net/minecraft/world/entity/EntityType",
                "java/lang/Object",
                &[],
            );
            class(
                "net/minecraft/resources/Identifier",
                "java/lang/Object",
                &[],
            );
            class(
                "net/minecraft/network/RegistryFriendlyByteBuf",
                "java/lang/Object",
                &[],
            );
            class(
                "net/minecraft/network/FriendlyByteBuf",
                "java/lang/Object",
                &[],
            );
            class(
                "net/minecraft/network/codec/StreamCodec",
                "java/lang/Object",
                &[],
            );
            class(
                "net/minecraft/network/protocol/common/custom/CustomPacketPayload",
                "java/lang/Object",
                &[],
            );
            class(
                "net/minecraft/network/protocol/common/custom/CustomPacketPayload$Type",
                "java/lang/Object",
                &[("id", "()Lnet/minecraft/resources/Identifier;", 0x0001)],
            );
            jar.finish().expect("finish the game jar");
        }

        // The target names the NeoForge capability plus the game types the
        // emitted shims construct, so `generate` has to materialize both.
        let target = root.join("demo-mod.jar");
        {
            let file = fs::File::create(&target).expect("create the target jar");
            let mut jar = zip::ZipWriter::new(file);
            let mut pool = PoolBuilder::default();
            pool.method(
                "net/neoforged/neoforge/items/IItemHandler",
                "getSlots",
                "()I",
            );
            pool.method("net/minecraft/world/item/ItemStack", "getCount", "()I");
            pool.method(
                "net/neoforged/neoforge/items/IItemHandler",
                "horizonUsesDirection",
                "(Lnet/minecraft/core/Direction;)V",
            );
            let bytes = pool.build("demo/Probe", "java/lang/Object", &[("run", &[0xb1])]);
            jar.start_file("demo/Probe.class", zip::write::SimpleFileOptions::default())
                .expect("start the target class");
            std::io::Write::write_all(&mut jar, &bytes).expect("write the target class");
            jar.finish().expect("finish the target jar");
        }

        let out = root.join("facade");
        let report = generate(&target, &game_jar, &[], &out, &tool("javac"), &tool("jar"))
            .unwrap_or_else(|e| {
                panic!(
                    "the NeoForge façade did not generate: {e} (sources in {})",
                    out.display()
                )
            });
        assert!(
            report.jar.is_file(),
            "the façade jar was not written: {report:?}"
        );
        assert!(
            out.join("src/net/neoforged/neoforge/items/IItemHandler.java")
                .is_file(),
            "the item-handler shim was not generated"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Every loader family the generator can emit, generated from a miniature
    /// 26.3 game jar and compiled as one source set. `javac` is the judge: a
    /// shim missing a brace, a record component the closure never emitted, a
    /// support type left out or a constructor a game stub does not have fails
    /// here, with the diagnostics in the error. The slices are: Fabric events,
    /// networking, client, screens, item groups, renderers, models, attachments,
    /// loader, datagen and menus; NeoForge's base, client, screen, registration,
    /// entity, model, attachment and menu surfaces; legacy Forge; and one target
    /// that mixes families and names a game *interface* (`Registry`), which a
    /// class-shaped shim must not assume it can construct or extend.
    #[test]
    fn every_facade_family_generates_and_compiles() {
        use crate::classes::tests_support::PoolBuilder;
        use std::{path::PathBuf, process::Command};

        let jdk_bin = std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"));
        let tool = |name: &str| {
            jdk_bin
                .as_ref()
                .map(|bin| bin.join(name))
                .unwrap_or_else(|| PathBuf::from(name))
        };
        let Ok(version) = Command::new(tool("javac")).arg("-version").output() else {
            return;
        };
        if !version.status.success() {
            return;
        }

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("horizon-port-pack-families-{stamp}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create the scratch root");

        let build_jar = |tag: &str, sources: &[(String, String)]| -> PathBuf {
            let src = root.join(format!("{tag}-src"));
            let classes = root.join(format!("{tag}-classes"));
            let mut files = Vec::new();
            for (relative, text) in sources {
                let file = src.join(relative);
                fs::create_dir_all(file.parent().expect("package directory"))
                    .expect("create the source package");
                fs::write(&file, text).expect("write the source");
                files.push(file);
            }
            fs::create_dir_all(&classes).expect("create the classes directory");
            let output = Command::new(tool("javac"))
                .arg("-nowarn")
                .arg("-d")
                .arg(&classes)
                .args(&files)
                .output()
                .expect("run javac");
            assert!(
                output.status.success(),
                "{tag} sources did not compile:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let jar = root.join(format!("{tag}.jar"));
            let output = Command::new(tool("jar"))
                .arg("--create")
                .arg("--file")
                .arg(&jar)
                .arg("-C")
                .arg(&classes)
                .arg(".")
                .output()
                .expect("run jar");
            assert!(
                output.status.success(),
                "{tag} jar was not written:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            jar
        };

        // The miniature game jar: the real 26.3 shapes these families name, at
        // the fidelity each shim's signature and body needs. A class an emitted
        // shim constructs is a real class with that constructor; a record is a
        // record; a game interface stays an interface.
        // The miniature game jar: the real 26.3 shapes these families name, at
        // the fidelity each shim's signature and body needs. A class an emitted
        // shim constructs is a real class with that constructor; a record is a
        // record; a game interface stays an interface. Each row is the shape,
        // with the package derived from its path.
        let java = |path: &str, body: &str| -> (String, String) {
            let package = path
                .trim_end_matches(".java")
                .rsplit_once('/')
                .expect("a package directory")
                .0
                .replace('/', ".");
            (path.to_owned(), format!("package {package};\n{body}\n"))
        };
        let game_sources: Vec<(String, String)> = [
            (
                "net/minecraft/server/MinecraftServer.java",
                "public class MinecraftServer {}",
            ),
            (
                "net/minecraft/server/level/ServerPlayer.java",
                "public class ServerPlayer {}",
            ),
            (
                "net/minecraft/commands/Commands.java",
                "public class Commands { public enum CommandSelection { ALL, DEDICATED } }",
            ),
            (
                "net/minecraft/commands/CommandSourceStack.java",
                "public class CommandSourceStack {}",
            ),
            (
                "net/minecraft/commands/CommandBuildContext.java",
                "public interface CommandBuildContext {}",
            ),
            ("net/minecraft/client/Minecraft.java", "public class Minecraft {}"),
            (
                "net/minecraft/client/player/LocalPlayer.java",
                "public class LocalPlayer {}",
            ),
            (
                "net/minecraft/client/multiplayer/ClientPacketListener.java",
                "public class ClientPacketListener {}",
            ),
            (
                "net/minecraft/client/gui/GuiGraphicsExtractor.java",
                "public class GuiGraphicsExtractor {}",
            ),
            (
                "net/minecraft/client/render/RenderTickCounter.java",
                "public class RenderTickCounter {}",
            ),
            (
                "net/minecraft/client/KeyMapping.java",
                "public class KeyMapping {}",
            ),
            (
                "net/minecraft/client/gui/screens/Screen.java",
                "public class Screen {}",
            ),
            (
                "net/minecraft/client/gui/components/ClickableWidget.java",
                "public class ClickableWidget {}",
            ),
            (
                "net/minecraft/client/renderer/entity/EntityRendererProvider.java",
                "public interface EntityRendererProvider {}",
            ),
            (
                "net/minecraft/client/renderer/blockentity/BlockEntityRendererProvider.java",
                "public interface BlockEntityRendererProvider {}",
            ),
            (
                "net/minecraft/client/model/geom/ModelLayerLocation.java",
                "public class ModelLayerLocation {}",
            ),
            (
                "net/minecraft/client/model/geom/builders/LayerDefinition.java",
                "public class LayerDefinition {}",
            ),
            (
                "net/minecraft/client/resources/model/UnbakedModel.java",
                "public interface UnbakedModel {}",
            ),
            (
                "net/minecraft/client/resources/model/BakedModel.java",
                "public interface BakedModel {}",
            ),
            (
                "net/minecraft/client/resources/model/ModelLoader.java",
                "public class ModelLoader {}",
            ),
            (
                "net/minecraft/client/renderer/block/dispatch/ModelState.java",
                "public class ModelState {}",
            ),
            (
                "net/minecraft/world/item/ItemStack.java",
                "public class ItemStack { public ItemStack(Item item, int count) {} }",
            ),
            (
                "net/minecraft/world/item/Item.java",
                "public class Item { public interface TooltipContext {} }",
            ),
            (
                "net/minecraft/world/item/TooltipFlag.java",
                "public interface TooltipFlag {}",
            ),
            (
                "net/minecraft/world/item/CreativeModeTab.java",
                "public class CreativeModeTab {}",
            ),
            (
                "net/minecraft/core/component/DataComponentType.java",
                "public class DataComponentType {}",
            ),
            (
                "net/minecraft/core/Direction.java",
                "public enum Direction { DOWN, UP, NORTH, SOUTH, WEST, EAST }",
            ),
            (
                "net/minecraft/core/BlockPos.java",
                "public class BlockPos { public int getX() { return 0; } public int getY() { return 0; } public int getZ() { return 0; } }",
            ),
            (
                "net/minecraft/world/flag/FeatureFlagSet.java",
                "public class FeatureFlagSet {}",
            ),
            (
                "net/minecraft/resources/Identifier.java",
                "public class Identifier { private final String value; public Identifier() { this.value = \"\"; } public Identifier(String namespace, String path) { this.value = namespace + \":\" + path; } public String toString() { return value; } }",
            ),
            (
                "net/minecraft/resources/ResourceKey.java",
                "public class ResourceKey<T> { public ResourceKey() {} }",
            ),
            // 26.3's `Registry` is an interface: the shims that name it must not
            // assume they can construct or extend a class.
            (
                "net/minecraft/core/Registry.java",
                "public interface Registry<T> { default <V> V register(net.minecraft.resources.ResourceKey<V> key, net.minecraft.resources.Identifier id, V value) { return value; } default boolean containsKey(net.minecraft.resources.Identifier id) { return false; } default T get(net.minecraft.resources.Identifier id) { return null; } default net.minecraft.resources.Identifier getKey(Object value) { return null; } }",
            ),
            (
                "net/minecraft/network/chat/Component.java",
                "public interface Component {}",
            ),
            (
                "net/minecraft/network/protocol/common/custom/CustomPacketPayload.java",
                "public interface CustomPacketPayload { Type type(); record Type(net.minecraft.resources.Identifier id) {} }",
            ),
            (
                "net/minecraft/world/inventory/AbstractContainerMenu.java",
                "public class AbstractContainerMenu {}",
            ),
            (
                "net/minecraft/world/inventory/MenuType.java",
                "public class MenuType<T extends AbstractContainerMenu> { public MenuType(Object featureFlags, Object supplier) {} }",
            ),
            (
                "net/minecraft/world/MenuProvider.java",
                "public interface MenuProvider { net.minecraft.network.chat.Component getDisplayName(); }",
            ),
            (
                "net/minecraft/world/entity/player/Inventory.java",
                "public class Inventory {}",
            ),
            (
                "net/minecraft/world/entity/player/Abilities.java",
                "public class Abilities {}",
            ),
            (
                "net/minecraft/network/RegistryFriendlyByteBuf.java",
                "public class RegistryFriendlyByteBuf {}",
            ),
            (
                "net/minecraft/network/codec/StreamCodec.java",
                "public interface StreamCodec<B, V> {}",
            ),
            // The bounded entity closure the attachment slice implements on
            // game classes (`Entity implements AttachmentTarget`), plus the
            // living-entity value its health/attribute/hand accessors name.
            (
                "net/minecraft/world/entity/Entity.java",
                "public class Entity {}",
            ),
            (
                "net/minecraft/world/entity/LivingEntity.java",
                "public class LivingEntity extends Entity {}",
            ),
            (
                "net/minecraft/world/entity/Mob.java",
                "public class Mob extends LivingEntity {}",
            ),
            (
                "net/minecraft/world/entity/SpawnGroupData.java",
                "public interface SpawnGroupData {}",
            ),
            (
                "net/minecraft/world/effect/MobEffect.java",
                "public class MobEffect {}",
            ),
            (
                "net/minecraft/world/effect/MobEffectInstance.java",
                "public class MobEffectInstance {}",
            ),
            (
                "net/minecraft/world/entity/ai/attributes/AttributeInstance.java",
                "public class AttributeInstance {}",
            ),
            (
                "net/minecraft/world/entity/ai/attributes/AttributeMap.java",
                "public class AttributeMap {}",
            ),
            (
                "net/minecraft/world/entity/ai/navigation/PathNavigation.java",
                "public class PathNavigation {}",
            ),
            (
                "net/minecraft/world/entity/ai/goal/GoalSelector.java",
                "public class GoalSelector {}",
            ),
            (
                "net/minecraft/world/entity/EntityType.java",
                "public class EntityType {}",
            ),
            (
                "net/minecraft/world/entity/EntityDimensions.java",
                "public class EntityDimensions {}",
            ),
            (
                "net/minecraft/world/entity/MobCategory.java",
                "public enum MobCategory { MISC }",
            ),
            ("net/minecraft/world/level/Level.java", "public class Level {}"),
            // Fabric's `BlockColorProvider` names this view, not `Level`; the
            // colour shim's `world` parameter must carry the real descriptor.
            (
                "net/minecraft/world/level/BlockAndTintGetter.java",
                "public interface BlockAndTintGetter {}",
            ),
            (
                "net/minecraft/world/level/block/entity/BlockEntity.java",
                "public class BlockEntity {}",
            ),
            (
                "net/minecraft/world/level/block/entity/BlockEntityType.java",
                "public class BlockEntityType {}",
            ),
            (
                "net/minecraft/world/level/block/state/BlockState.java",
                "public class BlockState {}",
            ),
            (
                "net/minecraft/world/level/block/state/StateDefinition.java",
                "public class StateDefinition {}",
            ),
            (
                "net/minecraft/world/level/block/Block.java",
                "public class Block { public net.minecraft.world.level.block.state.StateDefinition stateDefinition; }",
            ),
            (
                "net/minecraft/world/level/material/FluidState.java",
                "public class FluidState {}",
            ),
            (
                "net/minecraft/nbt/CompoundTag.java",
                "public class CompoundTag {}",
            ),
            (
                "net/minecraft/world/phys/AABB.java",
                "public class AABB { public double minX, minY, minZ, maxX, maxY, maxZ; public AABB() {} public AABB(double x1, double y1, double z1, double x2, double y2, double z2) {} }",
            ),
            (
                "net/minecraft/world/phys/Vec3.java",
                "public class Vec3 {}",
            ),
        ]
        .into_iter()
        .map(|(path, body)| java(path, body))
        .collect();
        let game_jar = build_jar("game", &game_sources);

        // The dependency jars the shims name: Brigadier's dispatcher is a real
        // library class on javac's classpath, and so is the codec an attachment
        // registration keeps.
        let lib_sources: Vec<(String, String)> = [
            (
                "com/mojang/serialization/Codec.java",
                "public interface Codec<A> {}",
            ),
            (
                "com/mojang/brigadier/CommandDispatcher.java",
                "public class CommandDispatcher<S> {}",
            ),
        ]
        .into_iter()
        .map(|(path, body)| java(path, body))
        .collect();
        let lib_jar = build_jar("lib", &lib_sources);

        let scenarios: [(&str, &[&str], &[&str]); 15] = [
            (
                "fabric-events",
                &[
                    "net/fabricmc/fabric/api/event/lifecycle/v1/ServerLifecycleEvents",
                    "net/fabricmc/fabric/api/event/lifecycle/v1/ClientLifecycleEvents",
                    "net/fabricmc/fabric/api/event/player/ServerPlayerEvents",
                    "net/fabricmc/fabric/api/command/v2/CommandRegistrationCallback",
                ],
                &[
                    "net/fabricmc/fabric/api/event/Event.java",
                    "net/fabricmc/fabric/api/event/lifecycle/v1/ServerLifecycleEvents.java",
                    "net/fabricmc/fabric/api/event/lifecycle/v1/ClientLifecycleEvents.java",
                    "net/fabricmc/fabric/api/event/player/ServerPlayerEvents.java",
                    "net/fabricmc/fabric/api/command/v2/CommandRegistrationCallback.java",
                ],
            ),
            (
                "fabric-networking",
                &[
                    "net/fabricmc/fabric/api/networking/v1/PayloadTypeRegistry",
                    "net/fabricmc/fabric/api/networking/v1/PacketSender",
                    "net/fabricmc/fabric/api/networking/v1/ServerPlayNetworking",
                    "net/fabricmc/fabric/api/client/networking/v1/ClientPlayNetworking",
                ],
                &[
                    "net/fabricmc/fabric/api/networking/v1/PayloadTypeRegistry.java",
                    "net/fabricmc/fabric/api/networking/v1/PacketSender.java",
                    "net/fabricmc/fabric/api/networking/v1/ServerPlayNetworking.java",
                    "net/fabricmc/fabric/api/client/networking/v1/ClientPlayNetworking.java",
                ],
            ),
            (
                "fabric-client",
                &[
                    "net/fabricmc/fabric/api/client/rendering/v1/HudRenderCallback",
                    "net/fabricmc/fabric/api/client/event/lifecycle/v1/ClientTickEvents",
                    "net/fabricmc/fabric/api/client/keybinding/v1/KeyBindingHelper",
                    "net/fabricmc/fabric/api/client/item/tooltip/v1/ItemTooltipCallback",
                ],
                &[
                    "net/fabricmc/fabric/api/event/Event.java",
                    "net/fabricmc/fabric/api/client/rendering/v1/HudRenderCallback.java",
                    "net/fabricmc/fabric/api/client/event/lifecycle/v1/ClientTickEvents.java",
                    "net/fabricmc/fabric/api/client/keybinding/v1/KeyBindingHelper.java",
                    "net/fabricmc/fabric/api/client/item/tooltip/v1/ItemTooltipCallback.java",
                ],
            ),
            (
                "fabric-screens",
                &[
                    "net/fabricmc/fabric/api/client/screen/v1/ScreenEvents",
                    "net/fabricmc/fabric/api/client/screen/v1/Screens",
                ],
                &[
                    "net/fabricmc/fabric/api/event/Event.java",
                    "net/fabricmc/fabric/api/client/screen/v1/ScreenEvents.java",
                    "net/fabricmc/fabric/api/client/screen/v1/Screens.java",
                ],
            ),
            (
                "fabric-item-groups",
                &[
                    "net/fabricmc/fabric/api/itemgroup/v1/ItemGroupEvents",
                    "net/fabricmc/fabric/api/itemgroup/v1/FabricItemGroupEntries",
                ],
                &[
                    "net/fabricmc/fabric/api/event/Event.java",
                    "net/fabricmc/fabric/api/itemgroup/v1/ItemGroupEvents.java",
                    "net/fabricmc/fabric/api/itemgroup/v1/FabricItemGroupEntries.java",
                ],
            ),
            (
                "fabric-renderers",
                &[
                    "net/fabricmc/fabric/api/client/rendering/v1/EntityRendererRegistry",
                    "net/fabricmc/fabric/api/client/rendering/v1/BlockEntityRendererRegistry",
                    "net/fabricmc/fabric/api/client/rendering/v1/ModelLayerRegistry",
                    "net/fabricmc/fabric/api/client/rendering/v1/ColorProviderRegistry",
                ],
                &[
                    "net/fabricmc/fabric/api/client/rendering/v1/EntityRendererRegistry.java",
                    "net/fabricmc/fabric/api/client/rendering/v1/BlockEntityRendererRegistry.java",
                    "net/fabricmc/fabric/api/client/rendering/v1/ModelLayerRegistry.java",
                    "net/fabricmc/fabric/api/client/rendering/v1/ColorProviderRegistry.java",
                ],
            ),
            (
                "fabric-models",
                &[
                    "net/fabricmc/fabric/api/client/model/loading/v1/ModelLoadingRegistry",
                    "net/fabricmc/fabric/api/client/model/loading/v1/ModelResourceProvider",
                    "net/fabricmc/fabric/api/client/model/loading/v1/ModelVariantProvider",
                    "net/fabricmc/fabric/api/client/model/loading/v1/ModelAppender",
                ],
                &[
                    "net/fabricmc/fabric/api/client/model/loading/v1/ModelProviderContext.java",
                    "net/fabricmc/fabric/api/client/model/loading/v1/ModelResourceProvider.java",
                    "net/fabricmc/fabric/api/client/model/loading/v1/ModelVariantProvider.java",
                    "net/fabricmc/fabric/api/client/model/loading/v1/ModelAppender.java",
                    "net/fabricmc/fabric/api/client/model/loading/v1/ModelLoadingRegistry.java",
                ],
            ),
            (
                "fabric-attachments",
                &[
                    "net/fabricmc/fabric/api/attachment/v1/AttachmentType",
                    "net/fabricmc/fabric/api/attachment/v1/AttachmentTarget",
                    "net/fabricmc/fabric/api/attachment/v1/AttachmentRegistry",
                    "net/minecraft/world/entity/Entity",
                ],
                &[
                    "net/fabricmc/fabric/api/attachment/v1/AttachmentType.java",
                    "net/fabricmc/fabric/api/attachment/v1/AttachmentTarget.java",
                    "net/fabricmc/fabric/api/attachment/v1/AttachmentRegistry.java",
                    "net/minecraft/world/entity/Entity.java",
                ],
            ),
            (
                "fabric-loader",
                &["net/fabricmc/loader/api/FabricLoader"],
                &[
                    "net/fabricmc/loader/api/FabricLoader.java",
                    "net/fabricmc/loader/api/ModContainer.java",
                    "net/fabricmc/loader/api/EntrypointContainer.java",
                    "net/fabricmc/loader/api/ObjectShare.java",
                    "net/fabricmc/loader/api/metadata/ModMetadata.java",
                    "net/fabricmc/loader/api/metadata/Version.java",
                ],
            ),
            (
                "fabric-datagen",
                &["net/fabricmc/fabric/api/datagen/v1/DataGeneratorEntrypoint"],
                &[
                    "net/fabricmc/loader/api/ModContainer.java",
                    "net/fabricmc/fabric/api/datagen/v1/DataGeneratorEntrypoint.java",
                    "net/fabricmc/fabric/api/datagen/v1/FabricDataOutput.java",
                    "net/fabricmc/fabric/api/datagen/v1/provider/DataProvider.java",
                    "net/fabricmc/fabric/api/datagen/v1/provider/FabricRecipeProvider.java",
                    "net/fabricmc/fabric/api/datagen/v1/provider/FabricLanguageProvider.java",
                    "net/fabricmc/fabric/api/datagen/v1/provider/FabricModelProvider.java",
                    "net/fabricmc/fabric/api/datagen/v1/FabricDataGenerator.java",
                ],
            ),
            (
                "fabric-menus",
                &[
                    "net/fabricmc/fabric/api/menu/v1/ExtendedMenuType",
                    "net/fabricmc/fabric/api/menu/v1/ExtendedMenuProvider",
                    "net/fabricmc/fabric/api/screenhandler/v1/ExtendedScreenHandlerType",
                    "net/fabricmc/fabric/api/screenhandler/v1/ExtendedScreenHandlerFactory",
                ],
                &[
                    "net/fabricmc/fabric/api/menu/v1/ExtendedMenuType.java",
                    "net/fabricmc/fabric/api/menu/v1/ExtendedMenuProvider.java",
                    "net/fabricmc/fabric/api/screenhandler/v1/ExtendedScreenHandlerType.java",
                    "net/fabricmc/fabric/api/screenhandler/v1/ExtendedScreenHandlerFactory.java",
                ],
            ),
            (
                "neoforge",
                &[
                    "net/neoforged/fml/common/Mod",
                    "net/neoforged/bus/api/IEventBus",
                    "net/neoforged/bus/api/SubscribeEvent",
                    "net/neoforged/neoforge/registries/RegisterEvent",
                    "net/neoforged/neoforge/registries/DeferredRegister",
                    "net/neoforged/neoforge/registries/DeferredHolder",
                    "net/neoforged/neoforge/capabilities/CapabilityToken",
                    "net/neoforged/neoforge/capabilities/Capability",
                    "net/neoforged/neoforge/capabilities/CapabilityManager",
                    "net/neoforged/neoforge/items/IItemHandler",
                    "net/neoforged/neoforge/items/IItemHandlerModifiable",
                    "net/neoforged/fml/javafmlmod/FMLJavaModLoadingContext",
                    "net/neoforged/neoforge/network/event/RegisterPayloadHandlersEvent",
                    "net/neoforged/neoforge/network/registration/PayloadRegistrar",
                    "net/neoforged/neoforge/event/entity/EntityJoinLevelEvent",
                    "net/neoforged/neoforge/capabilities/RegisterCapabilitiesEvent",
                    "net/neoforged/neoforge/common/world/BiomeModifier",
                    "net/neoforged/neoforge/common/capabilities/IStorage",
                    "net/neoforged/neoforge/client/event/RegisterKeyMappingsEvent",
                    "net/neoforged/neoforge/client/event/ClientTickEvent",
                    "net/neoforged/neoforge/client/event/RenderGuiEvent",
                    "net/neoforged/neoforge/client/event/ScreenEvent",
                    "net/neoforged/neoforge/event/BuildCreativeModeTabContentsEvent",
                    "net/neoforged/neoforge/client/event/EntityRenderersEvent",
                    "net/neoforged/neoforge/client/event/ModelEvent",
                    "net/neoforged/neoforge/attachment/AttachmentType",
                    "net/neoforged/neoforge/network/IContainerFactory",
                    "net/neoforged/neoforge/network/IExtendedContainerFactory",
                    "net/neoforged/neoforge/common/extensions/IMenuTypeExtension",
                ],
                &[
                    "net/neoforged/fml/common/Mod.java",
                    "net/neoforged/bus/api/Event.java",
                    "net/neoforged/bus/api/IEventBus.java",
                    "net/neoforged/bus/api/SubscribeEvent.java",
                    "net/neoforged/neoforge/registries/RegisterEvent.java",
                    "net/neoforged/neoforge/registries/DeferredRegister.java",
                    "net/neoforged/neoforge/items/IItemHandler.java",
                    "net/neoforged/neoforge/common/util/LazyOptional.java",
                    "net/minecraftforge/eventbus/api/Event.java",
                    "net/neoforged/neoforge/client/event/RegisterKeyMappingsEvent.java",
                    "net/neoforged/neoforge/client/event/ClientTickEvent.java",
                    "net/neoforged/neoforge/client/event/RenderGuiEvent.java",
                    "net/neoforged/neoforge/client/event/ScreenEvent.java",
                    "net/neoforged/neoforge/event/BuildCreativeModeTabContentsEvent.java",
                    "net/neoforged/neoforge/client/event/EntityRenderersEvent.java",
                    "net/neoforged/neoforge/client/event/ModelEvent.java",
                    "net/neoforged/neoforge/event/entity/EntityJoinLevelEvent.java",
                    "net/neoforged/neoforge/capabilities/RegisterCapabilitiesEvent.java",
                    "net/neoforged/neoforge/attachment/AttachmentType.java",
                    "net/neoforged/neoforge/network/IContainerFactory.java",
                    "net/neoforged/neoforge/network/IExtendedContainerFactory.java",
                    "net/neoforged/neoforge/common/extensions/IMenuTypeExtension.java",
                ],
            ),
            (
                "legacy-forge",
                &[
                    "net/minecraftforge/eventbus/api/Event",
                    "net/minecraftforge/eventbus/api/IEventBus",
                    "net/minecraftforge/eventbus/api/SubscribeEvent",
                    "net/minecraftforge/common/MinecraftForge",
                    "net/minecraftforge/common/capabilities/CapabilityToken",
                    "net/minecraftforge/common/capabilities/ICapabilityProvider",
                    "net/minecraftforge/fml/javafmlmod/FMLJavaModLoadingContext",
                    "net/minecraftforge/fml/ModContainer",
                    "net/minecraftforge/network/NetworkRegistry",
                    "net/minecraftforge/network/simple/SimpleChannel",
                ],
                &[
                    "net/minecraftforge/eventbus/api/Event.java",
                    "net/minecraftforge/eventbus/api/IEventBus.java",
                    "net/minecraftforge/eventbus/api/SubscribeEvent.java",
                    "net/minecraftforge/eventbus/api/EventPriority.java",
                    "net/minecraftforge/common/MinecraftForge.java",
                    "net/minecraftforge/common/capabilities/CapabilityToken.java",
                    "net/minecraftforge/common/capabilities/ICapabilityProvider.java",
                    "net/minecraftforge/common/util/LazyOptional.java",
                    "net/minecraftforge/fml/javafmlmod/FMLJavaModLoadingContext.java",
                    "net/minecraftforge/fml/ModContainer.java",
                    "net/minecraftforge/network/NetworkRegistry.java",
                    "net/minecraftforge/network/simple/SimpleChannel.java",
                ],
            ),
            // The three legacy surfaces named in the shim audit, in one target:
            // a Forge channel factory, NeoForge's removed `IStorage` marker and
            // Fabric's colour registry. Two stay object-shaped on purpose and
            // must appear in `Report::legacy`; the colour registry must carry
            // its typed descriptors. javac is the judge that they still compile
            // together on a partial game jar.
            (
                "legacy-shims",
                &[
                    "net/minecraftforge/network/NetworkRegistry",
                    "net/minecraftforge/network/simple/SimpleChannel",
                    "net/neoforged/neoforge/common/capabilities/IStorage",
                    "net/fabricmc/fabric/api/client/rendering/v1/ColorProviderRegistry",
                ],
                &[
                    "net/minecraftforge/network/NetworkRegistry.java",
                    "net/minecraftforge/network/simple/SimpleChannel.java",
                    "net/neoforged/neoforge/common/capabilities/IStorage.java",
                    "net/fabricmc/fabric/api/client/rendering/v1/ColorProviderRegistry.java",
                ],
            ),
            (
                "mixed",
                &[
                    "net/fabricmc/fabric/api/event/lifecycle/v1/ServerLifecycleEvents",
                    "net/fabricmc/fabric/api/networking/v1/ServerPlayNetworking",
                    "net/fabricmc/fabric/api/client/rendering/v1/HudRenderCallback",
                    "net/fabricmc/fabric/api/itemgroup/v1/ItemGroupEvents",
                    "net/fabricmc/fabric/api/attachment/v1/AttachmentRegistry",
                    "net/fabricmc/fabric/api/menu/v1/ExtendedMenuProvider",
                    "net/fabricmc/fabric/api/datagen/v1/DataGeneratorEntrypoint",
                    "net/neoforged/neoforge/network/registration/PayloadRegistrar",
                    "net/neoforged/neoforge/registries/RegisterEvent",
                    // Names the entity-event unit without the join event, so
                    // the seeded Entity/Level are exercised on that path too.
                    "net/neoforged/neoforge/capabilities/RegisterCapabilitiesEvent",
                    "net/minecraft/world/entity/Entity",
                    "net/minecraft/world/entity/LivingEntity",
                    "net/minecraft/world/entity/Mob",
                    "net/minecraft/core/Registry",
                ],
                &[
                    "net/fabricmc/fabric/api/event/Event.java",
                    "net/fabricmc/fabric/api/event/lifecycle/v1/ServerLifecycleEvents.java",
                    "net/fabricmc/fabric/api/networking/v1/ServerPlayNetworking.java",
                    "net/fabricmc/fabric/api/client/rendering/v1/HudRenderCallback.java",
                    "net/fabricmc/fabric/api/itemgroup/v1/ItemGroupEvents.java",
                    "net/fabricmc/fabric/api/attachment/v1/AttachmentRegistry.java",
                    "net/fabricmc/fabric/api/menu/v1/ExtendedMenuProvider.java",
                    "net/fabricmc/fabric/api/datagen/v1/FabricDataGenerator.java",
                    "net/neoforged/neoforge/network/registration/PayloadRegistrar.java",
                    "net/neoforged/neoforge/registries/RegisterEvent.java",
                    "net/neoforged/neoforge/capabilities/RegisterCapabilitiesEvent.java",
                    "net/minecraft/core/Registry.java",
                    "net/minecraft/core/DefaultedRegistry.java",
                    "net/minecraft/world/entity/Entity.java",
                    "net/minecraft/world/entity/LivingEntity.java",
                    "net/minecraft/world/entity/Mob.java",
                ],
            ),
        ];
        for (name, refs, expected) in scenarios {
            let target = root.join(format!("{name}.jar"));
            {
                let file = fs::File::create(&target).expect("create the target jar");
                let mut jar = zip::ZipWriter::new(file);
                let mut pool = PoolBuilder::default();
                for owner in refs {
                    pool.method(owner, "horizonFamilyRef", "()V");
                }
                let bytes = pool.build("demo/Probe", "java/lang/Object", &[("run", &[0xb1])]);
                jar.start_file("demo/Probe.class", zip::write::SimpleFileOptions::default())
                    .expect("start the target class");
                std::io::Write::write_all(&mut jar, &bytes).expect("write the target class");
                jar.finish().expect("finish the target jar");
            }
            let out = root.join(format!("{name}-facade"));
            let report = generate(
                &target,
                &game_jar,
                std::slice::from_ref(&lib_jar),
                &out,
                &tool("javac"),
                &tool("jar"),
            )
            .unwrap_or_else(|e| {
                panic!(
                    "the {name} façade did not generate and compile: {e} (sources in {})",
                    out.display()
                )
            });
            assert!(
                report.jar.is_file(),
                "{name}: the façade jar was not written: {report:?}"
            );
            for file in expected {
                assert!(
                    out.join("src").join(file).is_file(),
                    "{name}: the shim {file} was not generated (sources in {})",
                    out.display()
                );
            }
            // A mod's compiled call site names the erased parameter types, so
            // these shims must carry them too: `Object` would fail the invoke
            // at runtime even though the shim compiles.
            let read = |file: &str| {
                fs::read_to_string(out.join("src").join(file))
                    .unwrap_or_else(|e| panic!("{name}: cannot read {file}: {e}"))
            };
            if name == "neoforge" {
                let deferred = read("net/neoforged/neoforge/registries/DeferredRegister.java");
                assert!(
                    deferred.contains("create(net.minecraft.core.Registry registry, String id)"),
                    "{name}: DeferredRegister lost its Registry descriptor"
                );
                assert!(
                    deferred.contains(
                        "create(net.minecraft.resources.ResourceKey registryKey, String id)"
                    ),
                    "{name}: DeferredRegister lost its ResourceKey descriptor"
                );
                let join = read("net/neoforged/neoforge/event/entity/EntityJoinLevelEvent.java");
                for member in [
                    "public net.minecraft.world.entity.Entity getEntity()",
                    "public net.minecraft.world.level.Level getLevel()",
                ] {
                    assert!(
                        join.contains(member),
                        "{name}: the join event lost {member}"
                    );
                }
            }
            if name == "legacy-forge" {
                let channel = read("net/minecraftforge/network/simple/SimpleChannel.java");
                assert!(
                    channel.contains(
                        "java.util.function.BiFunction encoder, java.util.function.BiFunction decoder, java.util.function.BiConsumer handler"
                    ),
                    "{name}: the channel registration lost its JDK descriptors"
                );
            }
            if name == "legacy-shims" {
                // The colour registry links: its provider and single-target
                // overloads name real 26.3 game classes, never `Object`.
                let colors =
                    read("net/fabricmc/fabric/api/client/rendering/v1/ColorProviderRegistry.java");
                for member in [
                    "int getColor(net.minecraft.world.level.block.state.BlockState state, net.minecraft.world.level.BlockAndTintGetter world, net.minecraft.core.BlockPos pos, int tintIndex);",
                    "public ColorProvider register(net.minecraft.world.level.block.Block target, ColorProvider provider) {",
                    "public ColorProvider register(net.minecraft.world.item.Item target, ColorProvider provider) {",
                ] {
                    assert!(
                        colors.contains(member),
                        "{name}: the colour registry lost {member}"
                    );
                }
                assert!(
                    !colors.contains("Object target") && !colors.contains("Object world"),
                    "{name}: the colour registry kept an Object parameter"
                );
                // The bounded legacy surfaces stayed bounded, and each is named
                // in the report instead of only in a comment.
                let registry = read("net/minecraftforge/network/NetworkRegistry.java");
                assert!(
                    registry.contains(
                        "newSimpleChannel(Object name, Object protocol, Object clientAccepted, Object serverAccepted)"
                    ),
                    "{name}: NetworkRegistry no longer carries the bounded legacy descriptor"
                );
                let storage = read("net/neoforged/neoforge/common/capabilities/IStorage.java");
                assert!(
                    storage.contains("readNBT(Object capability, T instance, Object side)")
                        && storage.contains(
                            "writeNBT(Object capability, T instance, Object side, Object nbt)"
                        ),
                    "{name}: IStorage no longer carries the bounded legacy descriptor"
                );
                let rows: Vec<(&str, &str)> = report
                    .legacy
                    .iter()
                    .map(|row| (row.class.as_str(), row.member.as_str()))
                    .collect();
                for row in [
                    (
                        "net/minecraftforge/network/NetworkRegistry",
                        "newSimpleChannel",
                    ),
                    (
                        "net/neoforged/neoforge/common/capabilities/IStorage",
                        "readNBT/writeNBT",
                    ),
                ] {
                    assert!(
                        rows.contains(&row),
                        "{name}: the bounded shim {row:?} is missing from Report::legacy ({rows:?})"
                    );
                }
                assert!(
                    !rows
                        .iter()
                        .any(|(class, _)| class.contains("ColorProviderRegistry")),
                    "{name}: a typed shim was reported as bounded"
                );
                // `IEventBus.register(Object)` is genuinely Object in the real
                // bus API; leaving it erased is the correct descriptor.
                let bus = read("net/neoforged/bus/api/IEventBus.java");
                assert!(
                    bus.contains("default IEventBus register(Object object)")
                        && bus.contains("default IEventBus unregister(Object object)"),
                    "{name}: the event bus lost its real Object listeners"
                );
                // `Capability#orEmpty(R, Object)` is genuinely Object in the
                // real Forge API; typing it would change the invoke descriptor.
                let capability = read("net/minecraftforge/common/capabilities/Capability.java");
                assert!(
                    capability.contains("orEmpty(R value, Object requested)"),
                    "{name}: Capability lost its real Object request marker"
                );
            }
            if name == "mixed" {
                // `Registry<T>#getKey(T)` erases to Object on the real class;
                // typing this would change the descriptor a jar invokes.
                let registry = read("net/minecraft/core/Registry.java");
                assert!(
                    registry.contains("getKey(java.lang.Object")
                        || registry.contains("getKey(Object value)"),
                    "{name}: Registry lost its erased getKey descriptor"
                );
            }
        }
        let _ = fs::remove_dir_all(&root);
    }
}

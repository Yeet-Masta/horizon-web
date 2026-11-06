//! Client-side native Java mod bootstrap.
//!
//! A jar's entrypoints run in the bounded native interpreter; the typed
//! declarations they record are converted into the current content model
//! (`horizon_mods::native`), validated like a content mod's, and handed to
//! `mods::content_root_with_java` before the resources load.
use super::execution_role::ExecutionRole;
use horizon_client::mod_content::ContentMod;
use horizon_mods::native::NativeDeclarations;
use std::{
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum JavaLoader {
    Fabric,
    NeoForge,
    LegacyForge,
}
impl JavaLoader {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Fabric => "fabric",
            Self::NeoForge => "neoforge",
            Self::LegacyForge => "legacy-forge",
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum JavaEngine {
    Native,
    NativeQualified,
    Refused,
}
impl JavaEngine {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::NativeQualified => "native-qualified",
            Self::Refused => "refused",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct JavaCompatibilityReport {
    pub(super) jar: PathBuf,
    pub(super) loader: Option<JavaLoader>,
    pub(super) engine: JavaEngine,
    pub(super) entrypoints: usize,
    pub(super) registrations: usize,
    pub(super) opaque: usize,
    pub(super) isolation_failure: Option<String>,
}

static RUNTIMES: OnceLock<
    Mutex<Vec<(horizon_native_java::NativeJar, horizon_native_java::StdHost)>>,
> = OnceLock::new();
fn runtimes() -> &'static Mutex<Vec<(horizon_native_java::NativeJar, horizon_native_java::StdHost)>>
{
    RUNTIMES.get_or_init(|| Mutex::new(Vec::new()))
}

fn loader(jar: &horizon_native_java::NativeJar) -> Option<JavaLoader> {
    if jar.fabric_id.is_some()
        || !jar.fabric_entrypoints.is_empty()
        || jar.resource_names().any(|n| n == "fabric.mod.json")
    {
        Some(JavaLoader::Fabric)
    } else if jar.neoforge_id.is_some()
        || !jar.neoforge_entrypoints.is_empty()
        || jar
            .resource_names()
            .any(|n| n == "META-INF/neoforge.mods.toml")
    {
        Some(JavaLoader::NeoForge)
    } else if jar.resource_names().any(|n| n == "META-INF/mods.toml") {
        Some(JavaLoader::LegacyForge)
    } else {
        None
    }
}

fn mod_id(jar: &horizon_native_java::NativeJar, path: &std::path::Path) -> Option<String> {
    jar.native_id
        .clone()
        .or_else(|| jar.fabric_id.clone())
        .or_else(|| jar.neoforge_id.clone())
        .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
}

fn mod_version(jar: &horizon_native_java::NativeJar) -> &str {
    jar.fabric_version
        .as_deref()
        .or(jar.neoforge_version.as_deref())
        .unwrap_or("unknown")
}

/// Converts typed Java registrations into the declaration vocabulary the
/// content loader understands. Opaque registrations carry no declaration and
/// never enter content; the caller counts them for diagnostics.
pub(super) fn declarations_from_entries(
    entries: &[horizon_native_java::RegistryEntry],
) -> Result<NativeDeclarations, String> {
    use horizon_native_java::RegistryDeclaration;
    let mut declarations = NativeDeclarations::default();
    for entry in entries {
        let Some(declaration) = &entry.declaration else {
            continue;
        };
        match declaration {
            RegistryDeclaration::Block {
                template,
                properties,
                item,
            } => declarations.add_block(&entry.id, template, properties, *item)?,
            RegistryDeclaration::Item {
                template,
                properties,
            } => declarations.add_item(&entry.id, template, properties)?,
            RegistryDeclaration::MobEffect { category, color } => {
                declarations.add_effect(&entry.id, category, *color)?
            }
            RegistryDeclaration::Recipe { json } => declarations.add_recipe(&entry.id, json)?,
            RegistryDeclaration::Ingredient { json } => {
                declarations.add_ingredient(&entry.id, json)?
            }
            RegistryDeclaration::Custom { kind, properties } => {
                match kind.to_ascii_lowercase().as_str() {
                    "entity" | "entity_type" | "entity-type" => {
                        declarations.add_entity(&entry.id, properties)?
                    }
                    "menu" | "menus" => declarations.add_menu(&entry.id, properties)?,
                    "block_entity" | "block-entity" | "blockentity" => {
                        declarations.add_block_entity(&entry.id, properties)?
                    }
                    _ => declarations.add_custom(&entry.registry, &entry.id, kind, properties)?,
                }
            }
        }
    }
    Ok(declarations)
}

/// The content mods one jar declares, sorted by namespace. Each group is
/// checked by [`NativeDeclarations::content_mods`]; a jar whose declarations
/// name two namespaces yields one content mod per namespace, as the native
/// code path does.
#[cfg(test)]
pub(super) fn content_mods(
    entries: &[horizon_native_java::RegistryEntry],
    metadata: &[horizon_native_java::RegistryEntry],
) -> Result<Vec<ContentMod>, String> {
    let mut declarations = declarations_from_entries(entries)?;
    declarations.merge(declarations_from_entries(metadata)?)?;
    Ok(declarations
        .content_mods()?
        .into_iter()
        .map(|(id, content)| ContentMod { id, content })
        .collect())
}

fn run_one(
    path: &std::path::Path,
    role: ExecutionRole,
) -> Result<(Vec<ContentMod>, JavaCompatibilityReport, NativeDeclarations), String> {
    let jar = horizon_native_java::NativeJar::from_path(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if jar.client_side_only && !role.is_client() {
        return Err(format!(
            "{}: clientSideOnly mod cannot run for server execution",
            path.display()
        ));
    }
    let kinds = if role.is_client() {
        ["client", "main", "neoforge"]
    } else {
        ["server", "main", "neoforge"]
    };
    let (kind, entries) = kinds
        .iter()
        .find_map(|kind| {
            let e = jar.discover_entrypoints(kind);
            (!e.is_empty()).then_some((*kind, e))
        })
        .ok_or_else(|| format!("{}: no native entrypoint", path.display()))?;
    if entries.iter().any(|e| e.adapter.is_some()) {
        return Err(format!(
            "{}: unsupported Fabric entrypoint adapter",
            path.display()
        ));
    }
    let weave = jar.plan_load_time_weave(if role.is_client() {
        horizon_native_java::MixinSide::Client
    } else {
        horizon_native_java::MixinSide::Server
    });
    let jar = jar
        .with_woven_classes(&weave.classes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut host = horizon_native_java::StdHost::new().with_environment_type(if role.is_client() {
        "CLIENT"
    } else {
        "SERVER"
    });
    if let Some(id) = mod_id(&jar, path) {
        host = host.with_mod(id, mod_version(&jar));
    }
    for dependency in &jar.required_mods {
        host = host.with_mod(dependency, "unknown");
    }
    let mut host = jar.seed_host(host);
    for entry in &entries {
        jar.execute(&entry.class, entry.method.as_deref(), kind, &mut host)
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let typed: Vec<horizon_native_java::RegistryEntry> = host
        .registrations()
        .iter()
        .filter(|entry| entry.declaration.is_some())
        .cloned()
        .collect();
    // The jar's own `META-INF/horizon-native.json` rows are the explicit
    // adapter for a callback the interpreter could not type; merge them
    // transactionally so a conflicting id stops this jar, not the game.
    let mut declarations = declarations_from_entries(&typed)?;
    declarations.merge(declarations_from_entries(&jar.native_declarations)?)?;
    // The current content model has no table for these yet; name them so a
    // mod's rows are not silently dropped.
    if !declarations.block_entities.is_empty()
        || !declarations.recipes.is_empty()
        || !declarations.ingredients.is_empty()
    {
        log::warn!(
            "Horizon: Java mod {} declares {} block entity, {} recipe and {} ingredient row(s) the content model does not express yet",
            path.display(),
            declarations.block_entities.len(),
            declarations.recipes.len(),
            declarations.ingredients.len()
        );
    }
    let content: Vec<ContentMod> = declarations
        .content_mods()?
        .into_iter()
        .map(|(id, content)| ContentMod { id, content })
        .collect();
    let report = JavaCompatibilityReport {
        jar: path.to_path_buf(),
        loader: loader(&jar),
        engine: if weave.skipped.is_empty() {
            JavaEngine::Native
        } else {
            JavaEngine::NativeQualified
        },
        entrypoints: entries.len(),
        registrations: host.registrations().len(),
        opaque: host
            .registrations()
            .iter()
            .filter(|e| e.declaration.is_none())
            .count(),
        isolation_failure: None,
    };
    runtimes().lock().unwrap().push((jar, host));
    Ok((content, report, declarations))
}

pub(super) fn run_java_mods(
    paths: &[PathBuf],
    role: ExecutionRole,
) -> Result<(Vec<ContentMod>, Vec<JavaCompatibilityReport>), String> {
    runtimes().lock().unwrap().clear();
    let mut content = Vec::new();
    let mut reports = Vec::new();
    let mut declarations = Vec::new();
    for path in paths {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_one(path, role))) {
            Ok(Ok((c, r, d))) => {
                content.extend(c);
                reports.push(r);
                declarations.push(d);
            }
            Ok(Err(e)) => log::warn!("Horizon: Java mod {e} skipped"),
            Err(_) => log::warn!(
                "Horizon: Java mod {} panicked and was skipped",
                path.display()
            ),
        }
    }
    if let Err(e) = super::java_registry::publish(&declarations) {
        log::warn!("Horizon: Java registry snapshot not published: {e}");
    }
    if let Some(home) = std::env::var_os("HORIZON_HOME") {
        let jars = reports
            .iter()
            .map(|r| super::java_runtime_file::RuntimeJar {
                file: r
                    .jar
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                loader: r.loader.map_or("unknown", JavaLoader::name).to_owned(),
                engine: r.engine.name().to_owned(),
                woven: 0,
                skipped: 0,
                first_skip_reason: None,
                registrations: r.registrations,
                opaque: r.opaque,
                isolation_failure: r.isolation_failure.clone(),
            })
            .collect();
        let _ = super::java_runtime_file::write(
            &PathBuf::from(home),
            super::java_runtime_file::RuntimeReport {
                schema_version: super::java_runtime_file::SCHEMA_VERSION,
                jars,
            },
        );
    }
    Ok((content, reports))
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_native_java::{RegistryDeclaration, RegistryEntry};
    use std::collections::BTreeMap;

    fn block_entry(id: &str, template: &str) -> RegistryEntry {
        RegistryEntry::typed(
            "minecraft:block",
            id,
            RegistryDeclaration::Block {
                template: template.into(),
                properties: BTreeMap::from([("hardness".into(), "1.5".into())]),
                item: true,
            },
        )
    }

    #[test]
    fn a_java_block_declaration_becomes_a_content_mod() {
        let entries = [block_entry("fixture:ruby_block", "minecraft:stone")];
        let mods = content_mods(&entries, &[]).unwrap();
        assert_eq!(mods.len(), 1);
        assert_eq!(mods[0].id, "fixture");
        assert_eq!(mods[0].content.blocks.len(), 1);
        assert_eq!(mods[0].content.blocks[0].id, "fixture:ruby_block");
        assert_eq!(mods[0].content.blocks[0].template, "minecraft:stone");
        assert_eq!(mods[0].content.blocks[0].properties.hardness, Some(1.5));
        assert!(mods[0].content.blocks[0].item);
    }

    #[test]
    fn opaque_registrations_do_not_become_content() {
        let opaque = RegistryEntry {
            registry: "minecraft:block".into(),
            id: "fixture:opaque".into(),
            value: horizon_native_java::Value::Ref(Some("registered/opaque".into())),
            declaration: None,
        };
        assert!(content_mods(&[opaque], &[]).unwrap().is_empty());
    }

    #[test]
    fn metadata_declarations_merge_with_registered_ones() {
        let registered = [block_entry("fixture:ruby_block", "minecraft:stone")];
        let metadata = [RegistryEntry::typed(
            "minecraft:item",
            "fixture:ruby_gem",
            RegistryDeclaration::Item {
                template: "minecraft:diamond".into(),
                properties: BTreeMap::new(),
            },
        )];
        let mods = content_mods(&registered, &metadata).unwrap();
        assert_eq!(mods[0].content.blocks.len(), 1);
        assert_eq!(mods[0].content.items.len(), 1);
        assert_eq!(mods[0].content.items[0].id, "fixture:ruby_gem");
    }
}

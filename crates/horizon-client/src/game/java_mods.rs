//! Native Java mod bootstrap, in both execution roles.
//!
//! A jar's entrypoints run in the bounded native interpreter; the typed
//! declarations they record are converted into the current content model
//! (`horizon_mods::native`), validated like a content mod's, and handed to
//! `mods::content_root_with_java` before the resources load.
//!
//! The window's client role runs before its root is built. The integrated
//! server role runs on the server thread, after that root exists, so its rows
//! are published here instead ([`publish_server_declarations`]) and every
//! later content build carries them ([`server_content`]) until a reload puts
//! them in the root ([`mark_server_content_in_root`]).
use super::execution_role::ExecutionRole;
use horizon_client::mod_content::ContentMod;
use horizon_mods::native::NativeDeclarations;
use std::{
    path::PathBuf,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

/// The opt-in real-loader preflight. It lives beside this module because it
/// is the Java path's own diagnostic, but its file stays `game/fml.rs` as the
/// rest of the game's modules are laid out.
#[path = "fml.rs"]
pub(super) mod fml;

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

/// Why one jar never started: the path, and the runner's reason without the
/// path it already carries. A tolerant run logs it and plays on;
/// `HORIZON_JAVA_STRICT=1` returns it, so a strict server names the jar and
/// what was wrong with it instead of a bare path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct JavaJarFailure {
    pub(super) jar: PathBuf,
    pub(super) reason: String,
}

impl JavaJarFailure {
    /// The one line a log or a strict error carries: the jar, then why.
    pub(super) fn line(&self) -> String {
        format!("{}: {}", self.jar.display(), self.reason)
    }
}

/// One batch's result: the content root's rows, one report per jar that
/// started, the declarations a server role keeps for its reload, and why each
/// jar that did not start was skipped.
#[derive(Default)]
pub(super) struct JavaBatchOutcome {
    pub(super) content: Vec<ContentMod>,
    pub(super) reports: Vec<JavaCompatibilityReport>,
    pub(super) declarations: Vec<NativeDeclarations>,
    pub(super) failures: Vec<JavaJarFailure>,
}

impl JavaBatchOutcome {
    /// Logs one line per skipped jar, each after the caller's role prefix.
    pub(super) fn log_failures(&self, prefix: &str) {
        for failure in &self.failures {
            log::warn!("{prefix}{}", failure.line());
        }
    }
}

/// The last batch a role recorded for `server-status`: one report per jar that
/// started. The dedicated role and the integrated server thread share this
/// process, so a later batch replaces an earlier one.
static JAVA_REPORTS: Mutex<Vec<JavaCompatibilityReport>> = Mutex::new(Vec::new());

/// Records the batch a role's startup ran, replacing whatever was there.
pub(in crate::game) fn record_java_reports(reports: &[JavaCompatibilityReport]) {
    *JAVA_REPORTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = reports.to_vec();
}

/// The recorded batch, for `server-status`'s `java` section.
pub(in crate::game) fn java_reports() -> Vec<JavaCompatibilityReport> {
    JAVA_REPORTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

static RUNTIMES: OnceLock<
    Mutex<Vec<(horizon_native_java::NativeJar, horizon_native_java::StdHost)>>,
> = OnceLock::new();
fn runtimes() -> &'static Mutex<Vec<(horizon_native_java::NativeJar, horizon_native_java::StdHost)>>
{
    RUNTIMES.get_or_init(|| Mutex::new(Vec::new()))
}

/// The declarations a server role (the integrated server's thread, in
/// practice) recorded after the window built its content root, one set per
/// jar. They stay for every later build: a reload must not drop what the
/// server is playing with.
static SERVER_DECLARATIONS: Mutex<Vec<NativeDeclarations>> = Mutex::new(Vec::new());
/// Bumped when [`SERVER_DECLARATIONS`] gains rows: 0 means no server row ever
/// arrived, which is a run without `--java-mod`.
static SERVER_EPOCH: AtomicU64 = AtomicU64::new(0);
/// The last [`SERVER_EPOCH`] a content root was built with.
static ROOT_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Whether one jar recorded any registration at all.
fn has_rows(declarations: &NativeDeclarations) -> bool {
    !declarations.blocks.is_empty()
        || !declarations.items.is_empty()
        || !declarations.effects.is_empty()
        || !declarations.entities.is_empty()
        || !declarations.menus.is_empty()
        || !declarations.block_entities.is_empty()
        || !declarations.recipes.is_empty()
        || !declarations.ingredients.is_empty()
        || !declarations.custom.is_empty()
}

/// Publishes the declarations a server role recorded for the window's
/// content root.
///
/// Empty declarations (no `--java-mod`, or every jar refused) publish nothing
/// and bump no epoch, so a run without server Java rows never rebuilds a
/// resource root.
pub(super) fn publish_server_declarations(declarations: Vec<NativeDeclarations>) {
    if !declarations.iter().any(has_rows) {
        return;
    }
    let mut stored = SERVER_DECLARATIONS.lock().unwrap();
    if *stored == declarations {
        return;
    }
    *stored = declarations;
    SERVER_EPOCH.fetch_add(1, Ordering::Release);
}

/// The server-role content every content build layers, in load order.
pub(super) fn server_content() -> Vec<ContentMod> {
    let stored = SERVER_DECLARATIONS.lock().unwrap();
    let mut rows = Vec::new();
    for declarations in stored.iter() {
        match declarations.content_mods() {
            Ok(mods) => rows.extend(
                mods.into_iter()
                    .map(|(id, content)| ContentMod { id, content }),
            ),
            // The same conversion already ran per jar; a set that cannot be
            // rebuilt is left out with a line rather than stopping a reload.
            Err(e) => log::warn!("Horizon: integrated-server Java content skipped: {e}"),
        }
    }
    rows
}

/// A server role published rows after the window built its root: one rebuild
/// through the pack pipeline is owed.
pub(super) fn server_content_needs_root() -> bool {
    let epoch = SERVER_EPOCH.load(Ordering::Acquire);
    epoch != 0
        && epoch != ROOT_EPOCH.load(Ordering::Acquire)
        && !SERVER_DECLARATIONS.lock().unwrap().is_empty()
}

/// A content root was built with the published server rows: the rebuild the
/// next window frame asks for is no longer owed.
pub(super) fn mark_server_content_in_root() {
    ROOT_EPOCH.store(SERVER_EPOCH.load(Ordering::Acquire), Ordering::Release);
}

/// The Java tests of the several modules share these cells: they take turns.
#[cfg(test)]
pub(super) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Forgets the server rows and both epochs between tests.
#[cfg(test)]
pub(super) fn reset_server_content() {
    *SERVER_DECLARATIONS.lock().unwrap() = Vec::new();
    SERVER_EPOCH.store(0, Ordering::Release);
    ROOT_EPOCH.store(0, Ordering::Release);
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

fn handler_role(role: ExecutionRole) -> super::java_payloads::HandlerRole {
    match role {
        ExecutionRole::Client => super::java_payloads::HandlerRole::Client,
        ExecutionRole::IntegratedServer => super::java_payloads::HandlerRole::IntegratedServer,
        ExecutionRole::DedicatedServer => super::java_payloads::HandlerRole::DedicatedServer,
    }
}

fn run_one(
    path: &std::path::Path,
    role: ExecutionRole,
    payload_declarations: &mut Vec<super::java_payloads::PayloadDeclaration>,
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
    // Publish the jar's payload channels and keep its host before the runtimes
    // map takes ownership: dispatch resolves a marker through that host.
    super::java_payloads::adopt_host(handler_role(role), &jar, &mut host, payload_declarations);
    runtimes().lock().unwrap().push((jar, host));
    Ok((content, report, declarations))
}

/// The opt-in real-loader preflight (`fml`), logged once under the Java
/// compatibility prefix and run before the native path. It is skipped unless
/// `HORIZON_NEOFORGE_SERVER` and `HORIZON_JVM_BIN` are set, and its failure
/// only stops startup under `HORIZON_NEOFORGE_STRICT=1`.
pub(super) fn fml_preflight(
    role: ExecutionRole,
    jars: &[PathBuf],
) -> Result<Option<fml::FmlReport>, String> {
    let Some(outcome) = fml::preflight(role, jars) else {
        return Ok(None);
    };
    let level = if outcome.report.succeeded() {
        log::Level::Info
    } else {
        log::Level::Warn
    };
    for line in outcome.report.summary() {
        log::log!(level, "Horizon: Java compatibility: {line}");
    }
    match outcome.strict_error() {
        Some(error) => Err(error),
        None => Ok(Some(outcome.report)),
    }
}

/// The runner's error for one jar, without repeating the path the failure
/// already names.
fn jar_failure(path: &std::path::Path, error: String) -> JavaJarFailure {
    let prefix = format!("{}: ", path.display());
    let reason = match error.strip_prefix(&prefix) {
        Some(reason) => reason.to_owned(),
        None => error,
    };
    JavaJarFailure {
        jar: path.to_path_buf(),
        reason,
    }
}

/// Runs one batch of jars and keeps every jar's outcome, including why a jar
/// that never started was skipped ([`JavaBatchOutcome::failures`]). A caller
/// logs those under its own role's prefix, or a strict startup returns them so
/// the jar and its reason are named.
pub(super) fn run_java_mods_batch(
    paths: &[PathBuf],
    role: ExecutionRole,
) -> Result<JavaBatchOutcome, String> {
    runtimes().lock().unwrap().clear();
    super::java_payloads::clear_handler_jars_as(handler_role(role));
    let mut batch = JavaBatchOutcome::default();
    let mut payload_declarations = Vec::new();
    for path in paths {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_one(path, role, &mut payload_declarations)
        })) {
            Ok(Ok((c, r, d))) => {
                batch.content.extend(c);
                batch.reports.push(r);
                batch.declarations.push(d);
            }
            Ok(Err(error)) => batch.failures.push(jar_failure(path, error)),
            Err(_) => batch.failures.push(JavaJarFailure {
                jar: path.to_path_buf(),
                reason: "panicked and was skipped".to_owned(),
            }),
        }
    }
    if let Err(e) = super::java_registry::publish(&batch.declarations) {
        log::warn!("Horizon: Java registry snapshot not published: {e}");
    }
    if let Some(home) = std::env::var_os("HORIZON_HOME") {
        let jars = batch
            .reports
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
    Ok(batch)
}

/// The client role's batch: the content root's rows and one report per jar
/// that started. Skipped jars are logged here, with no role in the prefix.
pub(super) fn run_java_mods(
    paths: &[PathBuf],
    role: ExecutionRole,
) -> Result<(Vec<ContentMod>, Vec<JavaCompatibilityReport>), String> {
    let batch = run_java_mods_batch(paths, role)?;
    batch.log_failures("Horizon: Java mod skipped: ");
    Ok((batch.content, batch.reports))
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

    /// A server role's declarations ask for one content rebuild; with none,
    /// or the same batch again, the window owes none.
    #[test]
    fn server_declarations_ask_for_one_content_rebuild() {
        let _guard = test_lock();
        reset_server_content();
        assert!(!server_content_needs_root());
        let declarations =
            declarations_from_entries(&[block_entry("srvfix:bolt", "minecraft:stone")]).unwrap();
        publish_server_declarations(vec![declarations.clone()]);
        assert!(server_content_needs_root());
        assert_eq!(server_content()[0].id, "srvfix");
        publish_server_declarations(vec![declarations]);
        assert!(
            server_content_needs_root(),
            "a republish before the build owes the same one"
        );
        mark_server_content_in_root();
        assert!(!server_content_needs_root());
        let more =
            declarations_from_entries(&[block_entry("other:gem", "minecraft:stone")]).unwrap();
        publish_server_declarations(vec![more]);
        assert!(server_content_needs_root(), "a changed batch asks again");
        reset_server_content();
    }

    /// A jar that never started contributes no report, but a failure that
    /// names the jar and why: that is what a strict startup reports instead
    /// of a path it has to guess about.
    #[test]
    fn a_jar_that_never_started_carries_its_reason() {
        let _guard = test_lock();
        let path = PathBuf::from("/horizon-absent-mod.jar");
        let batch =
            run_java_mods_batch(std::slice::from_ref(&path), ExecutionRole::DedicatedServer)
                .unwrap();
        assert!(batch.reports.is_empty());
        assert!(batch.content.is_empty());
        assert_eq!(batch.failures.len(), 1);
        let failure = &batch.failures[0];
        assert_eq!(failure.jar, path);
        assert!(!failure.reason.is_empty());
        assert!(
            !failure.reason.contains("/horizon-absent-mod.jar"),
            "the jar is not repeated inside its own reason: {}",
            failure.reason
        );
        assert_eq!(
            failure.line(),
            format!("/horizon-absent-mod.jar: {}", failure.reason)
        );
    }

    /// The runner's message is built as `<path>: <why>`; the failure keeps the
    /// why alone, and an error that does not repeat the path keeps both.
    #[test]
    fn a_runner_error_keeps_its_reason_without_repeating_the_jar() {
        let path = std::path::Path::new("/mods/a.jar");
        let stripped = jar_failure(path, "/mods/a.jar: no native entrypoint".to_owned());
        assert_eq!(stripped.reason, "no native entrypoint");
        assert_eq!(stripped.jar, path);
        let kept = jar_failure(path, "zip error: bad header".to_owned());
        assert_eq!(kept.reason, "zip error: bad header");
    }

    /// A client role never reads the preflight environment, so a client
    /// startup answers "skipped" whatever the operator exported.
    #[test]
    fn the_fml_preflight_never_runs_for_a_client() {
        assert!(fml_preflight(ExecutionRole::Client, &[]).unwrap().is_none());
    }
}

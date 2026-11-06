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
//!
//! A batch bootstraps its jars on the process worker pool
//! (`horizon_threads::jobs`); the rows, reports and declarations one jar
//! produced merge back in path order, and the payload adoption that touches
//! shared state happens only there, one jar at a time.
//!
//! Before any jar starts, the batch builds one linked class table from every
//! supplied jar and the jars each nests in `META-INF/jars`/`META-INF/jarjar`:
//! that is the class path a loader gives the pack, and it is what lets one
//! jar's bytecode name a sibling library's class. The opt-in game-hook tier
//! (`HORIZON_JAVA_GAME_HOOKS`) binds game-class `@Inject` plans to the runtime's
//! host hook sites and keeps the callbacks for the game to fire.
use super::execution_role::ExecutionRole;
use horizon_client::mod_content::ContentMod;
use horizon_mods::native::NativeDeclarations;
use horizon_native_java::{GameHookRegistrations, LinkedClassTable, NativeJar, StdHost, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Cursor, Read},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

/// The opt-in real-loader preflight. It lives beside this module because it
/// is the Java path's own diagnostic, but its file stays `game/fml.rs` as the
/// rest of the game's modules are laid out.
#[path = "fml.rs"]
pub(super) mod fml;

/// The bounded on-disk cache of a jar's bootstrap result, so an unchanged jar
/// on a later start skips interpretation (`docs/java-mod-compat.md`).
#[path = "java_cache.rs"]
pub(super) mod java_cache;

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

    /// The loader a cache entry names, or `None` for a name this build does not
    /// know (a cache written by another engine is a miss anyway).
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "fabric" => Some(Self::Fabric),
            "neoforge" => Some(Self::NeoForge),
            "legacy-forge" => Some(Self::LegacyForge),
            _ => None,
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

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "native" => Some(Self::Native),
            "native-qualified" => Some(Self::NativeQualified),
            "refused" => Some(Self::Refused),
            _ => None,
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
    pub(super) mixin: JavaMixinSummary,
}

/// The load-time weave's counts for one jar, and the first skip a real loader would have applied.
/// The launcher reads the reason to say a jar needs the loader instead of running natively.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct JavaMixinSummary {
    pub(super) woven: usize,
    pub(super) skipped: usize,
    pub(super) first_skip_reason: Option<String>,
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

/// One jar's bootstrap result, before any process-wide registry adopts it.
///
/// The jar and its host travel back with the rows so the batch's serial
/// adoption step can publish the jar's payload channels (and keep the host
/// only if it declared any); nothing else needs the parsed classes after its
/// entrypoints ran. Both are `None` for a cached result: a jar that declared
/// payload channels is never cached, so there is nothing left to adopt.
struct JavaJarRun {
    jar: Option<horizon_native_java::NativeJar>,
    host: Option<horizon_native_java::StdHost>,
    content: Vec<ContentMod>,
    report: JavaCompatibilityReport,
    declarations: NativeDeclarations,
    /// The game-class callbacks the opt-in hook tier bound, kept with the jar
    /// so a host hook point can run them. `None` when the tier is off, or when
    /// the jar maps no plan onto a hook site.
    game_hooks: Option<GameHookRegistrations>,
}

/// Wall-clock time one jar's entrypoints may consume before the runner treats
/// the jar as failed.
///
/// A normal bootstrap registers its content in milliseconds, even on a loaded
/// machine, so the bound only catches a jar that loops or recurses without
/// returning. The batch's tolerant policy then skips that jar, naming it and
/// its budget in the failure line, while every later jar still runs.
const JAR_EXECUTION_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// Longest skip reason the runtime report carries, so a jar's own names cannot
/// flood the launcher's panel (which cuts every string it reads to 240).
const MAX_SKIP_REASON_CHARS: usize = 240;

/// The first skipped plan a real loader would apply: one whose family the
/// bounded weave does not run, or whose target is a game class. The launcher's
/// runtime row reads "target-outside-jar"/"net/minecraft/" and says the jar
/// needs the loader; a skip inside the jar's own classes says nothing.
fn first_game_skip_reason(skipped: &[horizon_native_java::SkippedMixinPlan]) -> Option<String> {
    skipped
        .iter()
        .find(|plan| {
            plan.reason == horizon_native_java::MixinSkipReason::UnsupportedFamily
                || plan.reason == horizon_native_java::MixinSkipReason::TargetOutsideJar
                || plan.target.owner.starts_with("net/minecraft/")
        })
        .map(|plan| {
            let mut reason = plan.to_string().replace(['\n', '\r'], " ");
            if reason.chars().count() > MAX_SKIP_REASON_CHARS {
                reason = reason.chars().take(MAX_SKIP_REASON_CHARS - 1).collect();
                reason.push('…');
            }
            reason
        })
}

/// One jar whose game-class callbacks the opt-in hook tier bound.
///
/// The jar and its host stay alive because [`NativeJar::run_game_hook`] resolves
/// a callback from the jar's own classes and lets it write through the host's
/// persistent class state, so a later firing sees what an earlier one wrote.
// A host hook point calls `run_java_game_hook` once the game's own sites are
// wired to this module (native_hooks.rs); until then the store has no reader.
#[allow(dead_code)]
struct JavaGameHookJar {
    jar: Arc<NativeJar>,
    registrations: GameHookRegistrations,
    host: StdHost,
}

/// The game-class callbacks one execution role's batch bound, in path order.
/// Keyed by role because the window and its server thread run different jars in
/// the same process, exactly as the payload handler jars are.
static JAVA_GAME_HOOKS: Mutex<BTreeMap<super::java_payloads::HandlerRole, Vec<JavaGameHookJar>>> =
    Mutex::new(BTreeMap::new());

/// What one firing of a host hook point did.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct JavaGameHookOutcome {
    /// How many callbacks ran; a cancellation stops the ones after it.
    pub(super) ran: usize,
    /// Whether a callback cancelled the vanilla work at this point.
    pub(super) cancelled: bool,
}

/// The environment variable that turns the game-class hook tier on. It is off
/// by default: the default weave keeps every game-class plan a skip, because a
/// host that does not fire the point must not claim it.
pub(super) const GAME_HOOKS_ENV: &str = "HORIZON_JAVA_GAME_HOOKS";

/// Whether the game-class hook tier is opted in.
pub(super) fn game_hook_tier_enabled() -> bool {
    game_hook_flag(std::env::var_os(GAME_HOOKS_ENV))
}

/// The spelling the flag accepts; unset, empty or any other value is off.
fn game_hook_flag(value: Option<std::ffi::OsString>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.to_string_lossy().trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        )
    })
}

/// Forgets the game-class callbacks of a previous bootstrap for one role.
pub(super) fn clear_java_game_hooks_as(role: super::java_payloads::HandlerRole) {
    JAVA_GAME_HOOKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&role);
}

/// Keeps one jar's bound callbacks for its role's host hook points.
///
/// The host is cloned after the entrypoints ran, so persistent class state and
/// the seeded mod class path travel with the jar. Only a jar that bound a plan
/// reaches here, so a pack without game-class mixins keeps nothing.
pub(super) fn publish_java_game_hooks(
    role: super::java_payloads::HandlerRole,
    jar: Arc<NativeJar>,
    registrations: GameHookRegistrations,
    host: &StdHost,
) {
    if registrations.is_empty() {
        return;
    }
    JAVA_GAME_HOOKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(role)
        .or_default()
        .push(JavaGameHookJar {
            jar,
            registrations,
            host: host.clone(),
        });
}

/// Runs every callback the role bound at one host hook point, in registration
/// order (Mixin apply order).
///
/// A point no jar registered at is `Ok(0)` rather than an error: the game may
/// fire a hook no mod uses. A callback that cancels stops the ones after it,
/// including those of later jars, which is what Mixin's `CallbackInfo.cancel`
/// does at one injection point.
#[allow(dead_code)]
pub(super) fn run_java_game_hook(
    role: super::java_payloads::HandlerRole,
    hook: &str,
    receiver: Option<Value>,
    args: &[Value],
) -> Result<JavaGameHookOutcome, String> {
    let mut store = JAVA_GAME_HOOKS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(jars) = store.get_mut(&role) else {
        return Ok(JavaGameHookOutcome::default());
    };
    let mut outcome = JavaGameHookOutcome::default();
    for entry in jars
        .iter_mut()
        .filter(|entry| !entry.registrations.entries(hook).is_empty())
    {
        let run = entry
            .jar
            .run_game_hook(
                &entry.registrations,
                hook,
                receiver.clone(),
                args,
                &mut entry.host,
            )
            .map_err(|error| error.to_string())?;
        outcome.ran += run.ran;
        if run.cancelled {
            outcome.cancelled = true;
            break;
        }
    }
    Ok(outcome)
}

/// The declaration rows of a cache entry, as the converter takes them.
fn cached_rows(rows: &[java_cache::Row]) -> Vec<horizon_native_java::RegistryEntry> {
    rows.iter().map(java_cache::Row::to_entry).collect()
}

/// A batch's declarations from one jar's row sets, in the order the live path
/// merges them: the host's typed rows, then the jar's own metadata rows.
fn cached_declarations(
    rows: &[java_cache::Row],
    metadata_rows: &[java_cache::Row],
) -> Result<NativeDeclarations, String> {
    let mut declarations = declarations_from_entries(&cached_rows(rows))?;
    declarations.merge(declarations_from_entries(&cached_rows(metadata_rows))?)?;
    Ok(declarations)
}

/// The report and rows of a jar that started on an earlier run, with nothing
/// interpreted: the declarations are replayed through the same converter, so a
/// cached jar and an interpreted one cannot disagree about what they declare.
fn run_from_cache(path: &std::path::Path, entry: &java_cache::Entry) -> Result<JavaJarRun, String> {
    let loader = match &entry.loader {
        None => None,
        Some(name) => Some(
            JavaLoader::from_name(name)
                .ok_or_else(|| format!("cached loader {name:?} is unknown"))?,
        ),
    };
    let engine = JavaEngine::from_name(&entry.report_engine)
        .ok_or_else(|| format!("cached engine {:?} is unknown", entry.report_engine))?;
    let declarations = cached_declarations(&entry.rows, &entry.metadata_rows)?;
    let content = declarations
        .content_mods()?
        .into_iter()
        .map(|(id, content)| ContentMod { id, content })
        .collect();
    let report = JavaCompatibilityReport {
        jar: path.to_path_buf(),
        loader,
        engine,
        entrypoints: entry.entrypoints,
        registrations: entry.registrations,
        opaque: entry.opaque,
        isolation_failure: entry.isolation_failure.clone(),
        mixin: JavaMixinSummary {
            woven: entry.woven,
            skipped: entry.skipped,
            first_skip_reason: entry.first_skip_reason.clone(),
        },
    };
    Ok(JavaJarRun {
        jar: None,
        host: None,
        content,
        report,
        declarations,
        // A cache entry carries rows, not the jar's game-class callbacks: a
        // jar that bound any is not stored (`cacheable`).
        game_hooks: None,
    })
}

/// What a successful bootstrap stores for the next start.
fn cache_entry(
    identity: &java_cache::Identity,
    role: ExecutionRole,
    typed: &[horizon_native_java::RegistryEntry],
    metadata: &[horizon_native_java::RegistryEntry],
    report: &JavaCompatibilityReport,
) -> java_cache::Entry {
    let rows: Vec<java_cache::Row> = typed
        .iter()
        .filter_map(java_cache::Row::from_entry)
        .collect();
    let metadata_rows: Vec<java_cache::Row> = metadata
        .iter()
        .filter_map(java_cache::Row::from_entry)
        .collect();
    java_cache::Entry {
        schema: java_cache::SCHEMA,
        sha256: identity.sha256.clone(),
        size: identity.size,
        role: java_cache::role_key(role).to_owned(),
        engine: java_cache::engine_key(),
        loader: report.loader.map(JavaLoader::name).map(str::to_owned),
        report_engine: report.engine.name().to_owned(),
        entrypoints: report.entrypoints,
        registrations: report.registrations,
        opaque: report.opaque,
        woven: report.mixin.woven,
        skipped: report.mixin.skipped,
        first_skip_reason: report.mixin.first_skip_reason.clone(),
        isolation_failure: report.isolation_failure.clone(),
        rows,
        metadata_rows,
    }
}

/// The batch's per-jar step with the pack's linked class table.
fn run_one_linked(
    path: &Path,
    role: ExecutionRole,
    table: &Arc<LinkedClassTable>,
) -> Result<JavaJarRun, String> {
    run_one_on(path, role, table, game_hook_tier_enabled())
}

fn run_one_on(
    path: &Path,
    role: ExecutionRole,
    table: &Arc<LinkedClassTable>,
    game_hooks: bool,
) -> Result<JavaJarRun, String> {
    // Decide from loader metadata whether this jar can run in this role at
    // all. Most jars in a real pack are client-only and never start on a
    // server; parsing their classes first was 6.5 s of an 11.2 s batch. Only
    // a jar whose entrypoints match the role is read again for execution.
    let metadata = horizon_native_java::NativeJar::from_path_metadata(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if metadata.client_side_only && !role.is_client() {
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
    let fabric_entrypoint = kinds
        .iter()
        .any(|kind| !metadata.discover_entrypoints(kind).is_empty());
    // Fabric names its entrypoints in metadata; Forge/NeoForge only marks the
    // class with `@Mod`, which the metadata read does not scan. A jar without
    // Fabric metadata (or with Forge/NeoForge metadata too) keeps the full
    // read so that discovery is exactly what it was.
    let needs_class_discovery = metadata.fabric_id.is_none()
        || metadata.neoforge_id.is_some()
        || metadata
            .resource_names()
            .any(|name| name == "META-INF/mods.toml" || name == "META-INF/neoforge.mods.toml");
    if !fabric_entrypoint && !needs_class_discovery {
        return Err(format!("{}: no native entrypoint", path.display()));
    }
    // A jar that ran before on this build, in this role, with these exact bytes
    // replays the rows it declared instead of being parsed and interpreted
    // again. The lookup is keyed by the jar's sha256 and length, both taken
    // from the file itself; failures fall through to the live path.
    //
    // The cache carries rows, not game-class callbacks, and an entry written
    // with the hook tier off says nothing about the plans it would bind. With
    // the tier on the jar is interpreted every start instead.
    let cached = if game_hooks {
        None
    } else {
        java_cache::lookup(path, role)
    };
    if let Some((_, Some(entry))) = &cached
        && let Ok(run) = run_from_cache(path, entry)
    {
        return Ok(run);
    }
    let run = bootstrap_jar(path, role, table, game_hooks)?;
    // Store the rows for the next start, unless the jar announced payload
    // channels or bound game-class callbacks: both live in the bootstrapped
    // host, which a later start cannot rebuild from rows.
    if let Some((identity, _)) = &cached
        && cacheable(&run)
    {
        let typed: Vec<horizon_native_java::RegistryEntry> =
            run.host.as_ref().map_or_else(Vec::new, |host| {
                host.registrations()
                    .iter()
                    .filter(|entry| entry.declaration.is_some())
                    .cloned()
                    .collect()
            });
        let entry = cache_entry(
            identity,
            role,
            &typed,
            &run.jar.as_ref().expect("just built").native_declarations,
            &run.report,
        );
        java_cache::remember(&entry);
    }
    Ok(run)
}

/// Reads, weaves and runs one jar against the pack's class path.
///
/// Separate from [`run_one_on`] so a test can exercise linking without the
/// on-disk cache, and so the cache decision stays a property of the run.
fn bootstrap_jar(
    path: &Path,
    role: ExecutionRole,
    table: &Arc<LinkedClassTable>,
    game_hooks: bool,
) -> Result<JavaJarRun, String> {
    let kinds = if role.is_client() {
        ["client", "main", "neoforge"]
    } else {
        ["server", "main", "neoforge"]
    };
    let jar = horizon_native_java::NativeJar::from_path(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
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
    let side = if role.is_client() {
        horizon_native_java::MixinSide::Client
    } else {
        horizon_native_java::MixinSide::Server
    };
    // The opt-in tier claims a game-class `@Inject` only when a host hook point
    // will fire it; the default weave keeps every such plan a skip.
    let weave = if game_hooks {
        jar.plan_load_time_weave_with_game_hooks(side)
    } else {
        jar.plan_load_time_weave(side)
    };
    let jar = jar
        .with_woven_classes(&weave.classes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let game_hook_registrations = if game_hooks {
        let registrations = jar
            .register_game_hooks(&weave)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if registrations.is_empty() {
            None
        } else {
            log::info!(
                "Horizon: Java mod {} binds {} game hook callback(s)",
                path.display(),
                registrations.len()
            );
            Some(registrations)
        }
    } else {
        None
    };
    // The pack's class path, shared by every host: a sibling jar's library is a
    // symbol this jar's bytecode may name. It is installed before `seed_host`
    // so the host's own seeding layers on top instead of replacing it.
    let mut host = horizon_native_java::StdHost::new()
        .with_environment_type(if role.is_client() { "CLIENT" } else { "SERVER" })
        .with_linked_class_table(Arc::clone(table));
    if let Some(id) = mod_id(&jar, path) {
        host = host.with_mod(id, mod_version(&jar));
    }
    for dependency in &jar.required_mods {
        host = host.with_mod(dependency, "unknown");
    }
    let mut host = jar.seed_host(host);
    // One budget for the jar, not one per entrypoint: several Fabric entrypoints
    // of one jar share the time the batch allows the jar as a whole.
    let budget = horizon_native_java::ExecutionBudget::within(JAR_EXECUTION_BUDGET);
    for entry in &entries {
        jar.execute_with_budget(
            &entry.class,
            entry.method.as_deref(),
            kind,
            &mut host,
            &budget,
        )
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
        mixin: JavaMixinSummary {
            woven: weave.woven.len()
                + weave.woven_anchors.len()
                + weave.woven_modify_args.len()
                + weave.woven_modify_variables.len(),
            skipped: weave.skipped.len(),
            first_skip_reason: first_game_skip_reason(&weave.skipped),
        },
    };
    // The jar and its host go back to the batch's serial adoption step, which
    // publishes its payload channels and keeps the host only if it declared
    // any. Nothing here touches process-wide state, so jars bootstrap in
    // parallel.
    Ok(JavaJarRun {
        jar: Some(jar),
        host: Some(host),
        content,
        report,
        declarations,
        game_hooks: game_hook_registrations,
    })
}

/// Whether a run may be stored for the next start. A jar that announced payload
/// channels or bound game-class callbacks needs the bootstrapped host, not just
/// its rows, so only a run without either is replayed.
fn cacheable(run: &JavaJarRun) -> bool {
    run.game_hooks.is_none()
        && run
            .host
            .as_ref()
            .is_some_and(|host| host.payload_registrations().is_empty())
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

/// Nested jars one outer jar may contribute to the pack class path.
const MAX_NESTED_JARS_PER_OUTER: usize = 64;
/// Uncompressed bytes one nested jar may occupy.
const MAX_NESTED_JAR_BYTES: u64 = 16 << 20;
/// Uncompressed bytes the whole batch's nested jars may occupy, matching the
/// resource budget a single jar's read already applies.
const MAX_NESTED_TOTAL_BYTES: u64 = 256 << 20;

/// The class path a loader gives one pack: every supplied jar's classes, plus
/// the classes of the jars each one nests in `META-INF/jars` (Fabric) or
/// `META-INF/jarjar` (NeoForge).
///
/// A pack keeps a mod's library in a separate jar — `xaerolib` beside the Xaero
/// maps, a config library beside the mod that names it — and Fabric's JiJ puts
/// it *inside* the mod jar. The native runtime never reads a process class path
/// or the filesystem, so this table is the class path: the first jar that
/// declares a name keeps it, exactly as a JVM's class path resolves a symbol.
///
/// One table is built per batch, before any jar bootstraps, because a jar may
/// name a sibling that appears later in path order.
fn pack_class_table(paths: &[PathBuf]) -> Arc<LinkedClassTable> {
    let mut table = LinkedClassTable::default();
    let mut linked = BTreeSet::new();
    let mut nested_total = 0u64;
    for path in paths {
        // The jar's file name is its class-path identity, so two copies of one
        // library keep one entry, as a class path would.
        let Some(id) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        if !linked.insert(id.clone()) {
            continue;
        }
        let jar = match NativeJar::from_path(path) {
            Ok(jar) => jar,
            Err(error) => {
                log::debug!(
                    "Horizon: Java class path skipped {}: {error}",
                    path.display()
                );
                continue;
            }
        };
        if let Err(error) = table.link_jar(id.clone(), &jar) {
            log::debug!("Horizon: Java class path skipped {id}: {error}");
            continue;
        }
        link_nested_jars(&mut table, path, &id, &mut nested_total);
    }
    log::info!(
        "Horizon: Java class path: {} class(es) from {} jar(s)",
        table.class_count(),
        table.jar_count()
    );
    Arc::new(table)
}

/// Adds the classes of the jars nested inside `path` to `table`.
///
/// The outer jar is already linked under `outer_id`; a nested jar is registered
/// as `outer_id!/<entry>`, so the table still names which jar supplied a class
/// for an error message. A bundled jar is a library: its `META-INF/services`
/// rows are not read (the interpreter resolves its classes, not its service
/// list), and it is never bootstrapped as a mod of its own.
fn link_nested_jars(
    table: &mut LinkedClassTable,
    path: &Path,
    outer_id: &str,
    nested_total: &mut u64,
) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        return;
    };
    let mut found = 0usize;
    for index in 0..archive.len() {
        let Ok(entry) = archive.by_index(index) else {
            continue;
        };
        let Ok(name) = entry.name() else {
            continue;
        };
        let name = name.into_owned();
        if !is_nested_jar_entry(&name) {
            continue;
        }
        if found >= MAX_NESTED_JARS_PER_OUTER {
            log::warn!(
                "Horizon: Java class path: {} nests more than {MAX_NESTED_JARS_PER_OUTER} jars; the rest are not linked",
                path.display()
            );
            break;
        }
        if entry.size() > MAX_NESTED_JAR_BYTES {
            log::warn!(
                "Horizon: Java class path: nested jar {name} in {} is larger than {MAX_NESTED_JAR_BYTES} bytes",
                path.display()
            );
            continue;
        }
        if nested_total.saturating_add(entry.size()) > MAX_NESTED_TOTAL_BYTES {
            log::warn!(
                "Horizon: Java class path: nested jars past the {MAX_NESTED_TOTAL_BYTES}-byte budget are not linked"
            );
            return;
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        if std::io::Read::take(entry, MAX_NESTED_JAR_BYTES + 1)
            .read_to_end(&mut bytes)
            .is_err()
            || bytes.len() as u64 > MAX_NESTED_JAR_BYTES
        {
            continue;
        }
        *nested_total = nested_total.saturating_add(bytes.len() as u64);
        found += 1;
        if let Err(error) =
            table.link_classes(format!("{outer_id}!/{name}"), nested_classes(&bytes))
        {
            log::debug!(
                "Horizon: Java class path skipped nested jar {name} in {}: {error}",
                path.display()
            );
        }
    }
}

/// Whether a zip entry is a jar a loader puts on the class path for this one:
/// Fabric's `META-INF/jars/` or NeoForge's `META-INF/jarjar/`.
fn is_nested_jar_entry(name: &str) -> bool {
    name.ends_with(".jar")
        && (name.starts_with("META-INF/jars/") || name.starts_with("META-INF/jarjar/"))
}

/// Whether a zip entry is a class the native runtime loads, matching the
/// entries a jar read parses.
fn is_java_class_entry(name: &str) -> bool {
    name.ends_with(".class")
        && !name.ends_with("module-info.class")
        && !name.ends_with("package-info.class")
        && !name.starts_with("META-INF/versions/")
}

/// The classes of one nested jar, parsed without its metadata: a bundled
/// library is linked for its bytecode, and one class this build cannot read is
/// skipped rather than losing the whole bundle.
fn nested_classes(bytes: &[u8]) -> Vec<horizon_native_java::ClassFile> {
    let Ok(mut archive) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return Vec::new();
    };
    let mut classes = Vec::new();
    for index in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(index) else {
            continue;
        };
        let Ok(name) = entry.name() else {
            continue;
        };
        let name = name.into_owned();
        if !is_java_class_entry(&name) {
            continue;
        }
        let mut bytes = Vec::new();
        if entry.read_to_end(&mut bytes).is_err() {
            continue;
        }
        if let Ok(class) = horizon_native_java::ClassFile::parse(&bytes) {
            classes.push(class);
        }
    }
    classes
}

/// Runs one batch of jars and keeps every jar's outcome, including why a jar
/// that never started was skipped ([`JavaBatchOutcome::failures`]). A caller
/// logs those under its own role's prefix, or a strict startup returns them so
/// the jar and its reason are named.
pub(super) fn run_java_mods_batch(
    paths: &[PathBuf],
    role: ExecutionRole,
) -> Result<JavaBatchOutcome, String> {
    let table = pack_class_table(paths);
    run_java_mods_batch_with(paths, role, move |path, role| {
        run_one_linked(path, role, &table)
    })
}

/// At most this many jars bootstrap at once. A started jar holds several times
/// its own size while it is parsed and its entrypoints run (the 500-jar
/// profile measured 6–10×), so handing every pool worker a jar would trade the
/// old serial run's retention for a larger parse-time peak. Eight saturates
/// zip decompression on a 16-thread machine without that.
const BOOTSTRAP_CONCURRENCY: usize = 8;

/// One batch's jobs on the process worker pool: paths are handed out by index
/// and each result lands in its index's slot, so the merge in
/// [`run_java_mods_batch_with`] sees path order whatever order the workers
/// finish in.
struct JarJobs<F> {
    paths: Vec<PathBuf>,
    role: ExecutionRole,
    run: Arc<F>,
    /// Jars that may bootstrap at once ([`BOOTSTRAP_CONCURRENCY`], or fewer on
    /// a small machine).
    cap: usize,
    state: Mutex<JarJobsState>,
    wake: Condvar,
}

struct JarJobsState {
    /// The next path index to hand out.
    next: usize,
    /// The next index the caller will adopt; earlier slots are already gone.
    consumed: usize,
    /// Jars bootstrapping now, never more than the cap.
    running: usize,
    slots: Vec<Option<Result<JavaJarRun, JavaJarFailure>>>,
}

impl<F> horizon_threads::jobs::Source for JarJobs<F>
where
    F: Fn(&std::path::Path, ExecutionRole) -> Result<JavaJarRun, String> + Send + Sync,
{
    fn next(&self) -> Option<horizon_threads::jobs::Rank> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // Never run further ahead than the window: a slow early jar must not
        // let the whole batch pile up behind it in the slots.
        let window = self.cap * 2;
        (state.next < self.paths.len()
            && state.running < self.cap
            && state.next < state.consumed + window)
            .then(|| {
                // Startup waits on this: urgent, and the index keeps start order.
                horizon_threads::jobs::Rank::new(
                    horizon_threads::jobs::Class::Urgent,
                    state.next as i64,
                )
            })
    }

    fn run(&self, _worker: usize) -> bool {
        let index = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.next >= self.paths.len() || state.running >= self.cap {
                return false;
            }
            state.running += 1;
            let index = state.next;
            state.next += 1;
            index
        };
        let outcome = bootstrap_one(&self.paths[index], self.role, &*self.run);
        let more = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.slots[index] = Some(outcome);
            state.running -= 1;
            self.wake.notify_all();
            state.next < self.paths.len() && state.next < state.consumed + self.cap * 2
        };
        if more {
            // This jar freed a place in the cap: wake a worker that found none.
            horizon_threads::jobs::pool().notify_all();
        }
        true
    }
}

impl<F> JarJobs<F> {
    /// Blocks until path `index` has a result and takes it. The caller adopts
    /// paths strictly in order, so a result never outlives its turn by more
    /// than the window.
    fn take(&self, index: usize) -> Result<JavaJarRun, JavaJarFailure> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        debug_assert_eq!(index, state.consumed);
        while state.slots[index].is_none() {
            state = self.wake.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        let outcome = state.slots[index].take().expect("the slot was just seen");
        state.consumed = index + 1;
        drop(state);
        // The consumer freed room in the window: workers may issue more.
        horizon_threads::jobs::pool().notify_all();
        outcome
    }
}

/// Runs one jar, turning a panic into that jar's own failure so the batch never
/// waits forever for a slot that a lost job would leave empty.
fn bootstrap_one<F>(
    path: &std::path::Path,
    role: ExecutionRole,
    run: &F,
) -> Result<JavaJarRun, JavaJarFailure>
where
    F: Fn(&std::path::Path, ExecutionRole) -> Result<JavaJarRun, String>,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(path, role))) {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(error)) => Err(jar_failure(path, error)),
        Err(_) => Err(JavaJarFailure {
            jar: path.to_path_buf(),
            reason: "panicked and was skipped".to_owned(),
        }),
    }
}

/// Bootstraps every path and hands each result to `accept` in path order, as
/// soon as the jars before it are done. The first result is adopted while the
/// rest still run, so a large pack never holds every parsed jar at once. More
/// than one jar runs on the process worker pool; the rows, reports and
/// declarations are independent per jar, so only the caller's merge is serial.
fn bootstrap_into<F, A>(paths: &[PathBuf], role: ExecutionRole, run: F, mut accept: A)
where
    F: Fn(&std::path::Path, ExecutionRole) -> Result<JavaJarRun, String> + Send + Sync + 'static,
    A: FnMut(Result<JavaJarRun, JavaJarFailure>),
{
    // One jar has nothing to parallelise, and starting the pool for it would
    // cost more than it saves.
    if paths.len() <= 1 {
        for path in paths {
            accept(bootstrap_one(path, role, &run));
        }
        return;
    }
    let pool = horizon_threads::jobs::pool();
    let jobs = Arc::new(JarJobs {
        paths: paths.to_vec(),
        role,
        run: Arc::new(run),
        cap: pool.threads().clamp(1, BOOTSTRAP_CONCURRENCY),
        state: Mutex::new(JarJobsState {
            next: 0,
            consumed: 0,
            running: 0,
            slots: (0..paths.len()).map(|_| None).collect(),
        }),
        wake: Condvar::new(),
    });
    let registration = pool.register(jobs.clone());
    registration.notify_all();
    for index in 0..paths.len() {
        accept(jobs.take(index));
    }
    // Also waits for any job still running, though every slot was taken.
    drop(registration);
}

/// Adopts one jar's bootstrap result into the batch and the role's shared
/// payload state, in path order: publishes its channels, keeps its host only
/// if it declared any, and drops the parsed jar.
fn adopt_run(
    batch: &mut JavaBatchOutcome,
    payload_declarations: &mut Vec<super::java_payloads::PayloadDeclaration>,
    role: ExecutionRole,
    outcome: Result<JavaJarRun, JavaJarFailure>,
) {
    let JavaJarRun {
        jar,
        host,
        content,
        report,
        declarations,
        game_hooks,
    } = match outcome {
        Ok(run) => run,
        Err(failure) => {
            batch.failures.push(failure);
            return;
        }
    };
    let id = handler_role(role);
    // A cached result has neither: a jar that declared payload channels was
    // never stored, so nothing is left to publish.
    if let (Some(jar), Some(mut host)) = (jar, host) {
        let adopted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            super::java_payloads::adopt_host(id, &jar, &mut host, payload_declarations);
        }));
        if adopted.is_err() {
            batch.failures.push(JavaJarFailure {
                jar: report.jar.clone(),
                reason: "panicked while its registrations were adopted".to_owned(),
            });
            return;
        }
        // Dispatch keeps the hosts that declared payload channels
        // (`java_payloads::store_handler_host_as`); the hook tier keeps the
        // jars that bound game-class callbacks. No other consumer needs the
        // parsed jar or its resources after its entrypoints ran, so do not hold
        // them for the process lifetime.
        if let Some(registrations) = game_hooks {
            publish_java_game_hooks(id, Arc::new(jar), registrations, &host);
        }
        drop(host);
    }
    batch.content.extend(content);
    batch.reports.push(report);
    batch.declarations.push(declarations);
}

/// The batch loop with its per-jar step injectable, so a test can prove that
/// two jars in one call both keep their declarations (in path order, whatever
/// order the workers finish in) without a real Java toolchain or corpus.
fn run_java_mods_batch_with<F>(
    paths: &[PathBuf],
    role: ExecutionRole,
    run: F,
) -> Result<JavaBatchOutcome, String>
where
    F: Fn(&std::path::Path, ExecutionRole) -> Result<JavaJarRun, String> + Send + Sync + 'static,
{
    super::java_payloads::clear_handler_jars_as(handler_role(role));
    clear_java_game_hooks_as(handler_role(role));
    let cache_before = java_cache::stats();
    let mut batch = JavaBatchOutcome::default();
    // The shared adoption step is serial, in path order: `adopt_host` mutates
    // the role's channel and handler maps, and a received frame answers from
    // the batch that came before it.
    let mut payload_declarations = Vec::new();
    bootstrap_into(paths, role, run, |outcome| {
        adopt_run(&mut batch, &mut payload_declarations, role, outcome);
    });
    // One line per batch: how much of the interpreter's work this start saved,
    // and how close the cache is to its bound. No batch ran a lookup when the
    // test seam replaced the per-jar step, so there is nothing to say (or prune)
    // then.
    let (hits, misses) = java_cache::stats();
    let hits = hits.saturating_sub(cache_before.0);
    let misses = misses.saturating_sub(cache_before.1);
    if hits + misses > 0 {
        log::info!(
            "Horizon: Java content cache: {hits} of {} jar(s) reused, {misses} bootstrapped",
            hits + misses
        );
        java_cache::prune();
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
                woven: r.mixin.woven,
                skipped: r.mixin.skipped,
                first_skip_reason: r.mixin.first_skip_reason.clone(),
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

    /// Every jar in one batch keeps its own rows, and the batch merges them in
    /// path order whatever order the workers finish in. A regression guard for
    /// the large-pack failure where only the last jar (or none) reached the
    /// content root: the batch accumulator, the per-namespace content mods and
    /// the registry snapshot must all carry both jars.
    #[test]
    fn two_jars_in_one_batch_both_contribute_declarations() {
        let _guard = test_lock();
        let paths = [
            PathBuf::from("/mods/first.jar"),
            PathBuf::from("/mods/second.jar"),
        ];
        let batch =
            run_java_mods_batch_with(&paths, ExecutionRole::DedicatedServer, |path, role| {
                assert_eq!(role, ExecutionRole::DedicatedServer);
                let namespace = path.file_stem().unwrap().to_string_lossy().into_owned();
                // The first jar finishes last, so a batch that merged in
                // completion order would put "second" first.
                std::thread::sleep(std::time::Duration::from_millis(if namespace == "first" {
                    60
                } else {
                    5
                }));
                let entries = [block_entry(
                    &format!("{namespace}:block"),
                    "minecraft:stone",
                )];
                let declarations = declarations_from_entries(&entries)?;
                let content = declarations
                    .content_mods()?
                    .into_iter()
                    .map(|(id, content)| ContentMod { id, content })
                    .collect();
                Ok(JavaJarRun {
                    jar: Some(horizon_native_java::NativeJar::default()),
                    host: Some(horizon_native_java::StdHost::new()),
                    content,
                    report: JavaCompatibilityReport {
                        jar: path.to_path_buf(),
                        loader: Some(JavaLoader::Fabric),
                        engine: JavaEngine::Native,
                        entrypoints: 1,
                        registrations: 1,
                        opaque: 0,
                        isolation_failure: None,
                        mixin: Default::default(),
                    },
                    declarations,
                    game_hooks: None,
                })
            })
            .unwrap();
        assert!(batch.failures.is_empty(), "{:?}", batch.failures);
        assert_eq!(batch.declarations.len(), 2, "one declaration set per jar");
        assert_eq!(batch.content.len(), 2, "one content mod per namespace");
        let ids: Vec<_> = batch.content.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["first", "second"], "content merges in path order");
        let reports: Vec<_> = batch
            .reports
            .iter()
            .map(|row| row.jar.file_stem().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(reports, ["first", "second"], "reports keep path order");
        for row in &batch.content {
            assert_eq!(row.content.blocks.len(), 1, "{} lost its block", row.id);
        }
        let snapshot = super::super::java_registry::snapshot();
        assert!(snapshot.blocks.iter().any(|id| id == "first:block"));
        assert!(snapshot.blocks.iter().any(|id| id == "second:block"));
    }

    /// A jar that exhausts its execution budget is one failure line: every
    /// other jar in the batch still starts and keeps its rows. This is the
    /// tolerant policy the wall-clock guard relies on to isolate one jar.
    #[test]
    fn a_budget_failure_does_not_stop_the_next_jar() {
        let _guard = test_lock();
        let paths = [
            PathBuf::from("/mods/spins.jar"),
            PathBuf::from("/mods/fine.jar"),
        ];
        let batch = run_java_mods_batch_with(&paths, ExecutionRole::DedicatedServer, |path, _| {
            if path.file_stem().unwrap() == "spins" {
                return Err(format!(
                    "{}: native interpreter exceeded its {:.1}s execution budget at demo/Spin#spin",
                    path.display(),
                    JAR_EXECUTION_BUDGET.as_secs_f64()
                ));
            }
            let entries = [block_entry("fine:block", "minecraft:stone")];
            let declarations = declarations_from_entries(&entries)?;
            let content = declarations
                .content_mods()?
                .into_iter()
                .map(|(id, content)| ContentMod { id, content })
                .collect();
            Ok(JavaJarRun {
                jar: Some(horizon_native_java::NativeJar::default()),
                host: Some(horizon_native_java::StdHost::new()),
                content,
                report: JavaCompatibilityReport {
                    jar: path.to_path_buf(),
                    loader: Some(JavaLoader::Fabric),
                    engine: JavaEngine::Native,
                    entrypoints: 1,
                    registrations: 1,
                    opaque: 0,
                    isolation_failure: None,
                    mixin: Default::default(),
                },
                declarations,
                game_hooks: None,
            })
        })
        .unwrap();
        assert_eq!(batch.failures.len(), 1, "{:?}", batch.failures);
        assert_eq!(batch.failures[0].jar, paths[0]);
        assert!(
            batch.failures[0].reason.contains("execution budget"),
            "{}",
            batch.failures[0].reason
        );
        assert_eq!(batch.content.len(), 1, "the second jar still started");
        assert_eq!(batch.content[0].id, "fine");
        assert_eq!(batch.reports.len(), 1);
        assert_eq!(batch.reports[0].jar, paths[1]);
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

    fn method_ref(owner: &str, name: &str) -> horizon_native_java::MethodRef {
        horizon_native_java::MethodRef {
            owner: owner.into(),
            name: name.into(),
            descriptor: "(I)V".into(),
        }
    }

    fn skipped_plan(
        owner: &str,
        reason: horizon_native_java::MixinSkipReason,
        at: &str,
    ) -> horizon_native_java::SkippedMixinPlan {
        horizon_native_java::SkippedMixinPlan {
            target: method_ref(owner, "tick"),
            callback: method_ref("demo/Mixin", "hook"),
            at: at.into(),
            reason,
        }
    }

    /// The runtime row names the first plan a real loader would apply: a game-class target or an
    /// unsupported family, never a skip inside the jar itself, and one bounded line.
    #[test]
    fn the_first_game_skip_reason_names_the_loader_work() {
        use horizon_native_java::MixinSkipReason;
        let own = skipped_plan("demo/Target", MixinSkipReason::Cancellable, "TAIL");
        let game = skipped_plan(
            "net/minecraft/world/level/Level",
            MixinSkipReason::TargetOutsideJar,
            "HEAD",
        );
        let text = first_game_skip_reason(&[own.clone(), game.clone()]).unwrap();
        assert!(text.starts_with("target-outside-jar HEAD net/minecraft/world/level/Level#tick"));
        assert!(text.contains("demo/Mixin#hook"), "{text}");
        assert!(
            first_game_skip_reason(&[own]).is_none(),
            "an in-jar skip needs no loader"
        );
        let unsupported = skipped_plan("demo/Target", MixinSkipReason::UnsupportedFamily, "INVOKE");
        assert_eq!(
            first_game_skip_reason(&[unsupported])
                .as_deref()
                .map(|r| r.split(' ').next().unwrap()),
            Some("unsupported-family")
        );
        let long = skipped_plan(
            "net/minecraft/A",
            MixinSkipReason::Weaver("x".repeat(1000)),
            "HEAD",
        );
        let bounded = first_game_skip_reason(&[long]).unwrap();
        assert_eq!(bounded.chars().count(), MAX_SKIP_REASON_CHARS);
        assert!(!bounded.contains(['\n', '\r']));
    }

    fn report_for(path: &str) -> JavaCompatibilityReport {
        JavaCompatibilityReport {
            jar: PathBuf::from(path),
            loader: Some(JavaLoader::Fabric),
            engine: JavaEngine::Native,
            entrypoints: 1,
            registrations: 1,
            opaque: 0,
            isolation_failure: None,
            mixin: Default::default(),
        }
    }

    /// A jar stored on one start replays exactly the declarations it recorded,
    /// through the same converter the live path uses: the content root of a
    /// warm start is the one the cold start built.
    #[test]
    fn a_cached_run_replays_the_rows_it_recorded() {
        let _guard = test_lock();
        let entries = [block_entry("fixture:ruby_block", "minecraft:stone")];
        let live = declarations_from_entries(&entries).unwrap();
        let identity = java_cache::Identity {
            sha256: "a".repeat(64),
            size: 1024,
        };
        let report = report_for("/mods/fixture.jar");
        let entry = cache_entry(
            &identity,
            ExecutionRole::DedicatedServer,
            &entries,
            &[],
            &report,
        );
        let cached = run_from_cache(std::path::Path::new("/mods/fixture.jar"), &entry).unwrap();
        assert_eq!(cached.declarations, live);
        assert_eq!(cached.content.len(), 1);
        assert_eq!(cached.content[0].id, "fixture");
        assert_eq!(cached.content[0].content.blocks.len(), 1);
        assert_eq!(cached.report, report);
        assert!(cached.jar.is_none() && cached.host.is_none());
        assert!(!cacheable(&cached), "a replayed run has no host to adopt");
    }

    /// The rows are the declaration input, so a cache entry that cannot rebuild
    /// them is a miss rather than a partly applied mod.
    #[test]
    fn a_cached_entry_with_a_bad_row_is_not_replayed() {
        let _guard = test_lock();
        let identity = java_cache::Identity {
            sha256: "b".repeat(64),
            size: 1024,
        };
        let mut entry = java_cache::Entry {
            schema: java_cache::SCHEMA,
            sha256: identity.sha256.clone(),
            size: identity.size,
            role: java_cache::role_key(ExecutionRole::Client).to_owned(),
            engine: java_cache::engine_key(),
            loader: Some("fabric".into()),
            report_engine: "native".into(),
            entrypoints: 1,
            registrations: 1,
            opaque: 0,
            woven: 0,
            skipped: 0,
            first_skip_reason: None,
            isolation_failure: None,
            rows: Vec::new(),
            metadata_rows: Vec::new(),
        };
        entry.rows.push(java_cache::Row {
            registry: "minecraft:block".into(),
            id: "fixture:broken".into(),
            declaration: java_cache::Declaration::Block {
                template: "minecraft:stone".into(),
                properties: std::collections::BTreeMap::from([(
                    "hardness".into(),
                    "not a number".into(),
                )]),
                item: true,
            },
        });
        assert!(run_from_cache(std::path::Path::new("/mods/fixture.jar"), &entry).is_err());
    }

    /// A jar that announced payload channels is never stored: its channels live
    /// in the bootstrapped host, which a later start cannot rebuild from rows.
    #[test]
    fn a_jar_with_payload_channels_is_not_cacheable() {
        use horizon_native_java::{Host, MethodRef, Value};
        let _guard = test_lock();
        let run = |host: horizon_native_java::StdHost| {
            let entries = [block_entry("fixture:ruby_block", "minecraft:stone")];
            let declarations = declarations_from_entries(&entries).unwrap();
            let content = declarations
                .content_mods()
                .unwrap()
                .into_iter()
                .map(|(id, content)| ContentMod { id, content })
                .collect();
            JavaJarRun {
                jar: Some(horizon_native_java::NativeJar::default()),
                host: Some(host),
                content,
                report: report_for("/mods/fixture.jar"),
                declarations,
                game_hooks: None,
            }
        };
        assert!(cacheable(&run(horizon_native_java::StdHost::new())));

        let mut host =
            horizon_native_java::StdHost::new().with_packet_type("payload#1", "demo:frame");
        Host::invoke(
            &mut host,
            &MethodRef {
                owner: "net/fabricmc/fabric/api/client/networking/v1/ClientPlayNetworking".into(),
                name: "registerGlobalReceiver".into(),
                descriptor: "(Ljava/lang/Object;Ljava/lang/Object;)Z".into(),
            },
            &[
                Value::Ref(Some("payload#1".into())),
                Value::Ref(Some("handler#1".into())),
            ],
        )
        .unwrap();
        assert!(
            !cacheable(&run(host)),
            "a jar with payload channels keeps being interpreted"
        );
    }

    /// The flag is opt-in: only an explicit yes turns the game-class hook tier
    /// on, so an unset or misspelled value keeps the default weave's refusals.
    #[test]
    fn the_game_hook_flag_accepts_only_an_explicit_yes() {
        for yes in ["1", "true", "on", "yes", " TRUE "] {
            assert!(
                game_hook_flag(Some(yes.into())),
                "{yes:?} should enable the tier"
            );
        }
        for no in ["", "0", "false", "off", "maybe"] {
            assert!(
                !game_hook_flag(Some(no.into())),
                "{no:?} should keep the tier off"
            );
        }
        assert!(!game_hook_flag(None), "unset is off");
    }

    /// Scratch stays under `~/horizon-corpus`, never the system temporary
    /// directory, so a test never touches a player's files.
    fn scratch_root(name: &str) -> PathBuf {
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("horizon-corpus")
            .join("opencode");
        let root = base.join(format!("java-mods-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a scratch root");
        root
    }

    fn javac() -> Option<PathBuf> {
        let candidate = std::env::var_os("JAVA_HOME")
            .map(|home| PathBuf::from(home).join("bin/javac"))
            .unwrap_or_else(|| PathBuf::from("javac"));
        if candidate.is_absolute() {
            return candidate.is_file().then_some(candidate);
        }
        let paths = std::env::var_os("PATH")?;
        std::env::split_paths(&paths)
            .map(|dir| dir.join(&candidate))
            .find(|path| path.is_file())
    }

    /// Compiles `sources` into `<root>/<name>/classes`, answering that
    /// directory. `None` when no JDK is installed, so a checkout without one
    /// still runs the rest of the suite.
    fn compile_java_classes(
        root: &Path,
        name: &str,
        sources: &[(&str, &str)],
        classpath: &[&Path],
    ) -> Option<PathBuf> {
        let javac = javac()?;
        let base = root.join(name);
        let classes = base.join("classes");
        std::fs::create_dir_all(&classes).ok()?;
        let mut files = Vec::new();
        for (relative, body) in sources {
            let path = base.join("src").join(relative);
            std::fs::create_dir_all(path.parent()?).ok()?;
            std::fs::write(&path, body).ok()?;
            files.push(path);
        }
        let mut command = std::process::Command::new(javac);
        command.arg("--release").arg("17").arg("-d").arg(&classes);
        if !classpath.is_empty() {
            command
                .arg("-classpath")
                .arg(std::env::join_paths(classpath).ok()?);
        }
        let output = command.args(&files).output().ok()?;
        assert!(
            output.status.success(),
            "javac failed for {name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Some(classes)
    }

    /// The `.class` entries under `dir`, named relative to it, sorted so a jar
    /// is byte-identical for the same sources.
    fn class_entries(dir: &Path) -> Vec<(String, Vec<u8>)> {
        fn walk(dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path
                    .extension()
                    .is_some_and(|extension| extension == "class")
                {
                    let name = path
                        .strip_prefix(dir)
                        .expect("a descendant")
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push((name, std::fs::read(&path).expect("a class file")));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, &mut out);
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn write_jar(path: &Path, entries: &[(String, Vec<u8>)]) {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(File::create(path).expect("a scratch jar file"));
        let options = zip::write::SimpleFileOptions::default();
        for (name, bytes) in entries {
            zip.start_file(name.as_str(), options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }

    /// A library jar and a mod jar compiled separately: the mod's bytecode
    /// names the library's class, and only the pack's linked table makes that
    /// symbol resolve. Without it the entrypoint reads `null/0/false`, the
    /// corpus's cross-jar symptom.
    const CROSS_JAR_LIBRARY: &[(&str, &str)] = &[(
        "lib/Config.java",
        "package lib;\n\
         public final class Config {\n\
             public static String name = \"linked\";\n\
             public static int number() { return 41; }\n\
             public static boolean flag = true;\n\
         }\n",
    )];

    const CROSS_JAR_MOD: &[(&str, &str)] = &[(
        "demo/Entry.java",
        "package demo;\n\
         import lib.Config;\n\
         public class Entry {\n\
             public static void onInitialize() {\n\
                 String seen = Config.name + \"/\" + Config.number() + \"/\" + Config.flag;\n\
                 if (seen.startsWith(\"null\") || seen.endsWith(\"false\")) {\n\
                     throw new IllegalStateException(seen);\n\
                 }\n\
             }\n\
         }\n",
    )];

    fn fabric_metadata(id: &str) -> Vec<u8> {
        format!(
            "{{\"schemaVersion\":1,\"id\":\"{id}\",\"version\":\"1.0.0\",\"entrypoints\":{{\"main\":\"demo.Entry\"}}}}"
        )
        .into_bytes()
    }

    /// Two supplied jars are one class path: the mod jar's entrypoint resolves
    /// the library jar's static field and method.
    #[test]
    fn two_jars_link_through_one_pack_table() {
        let root = scratch_root("two-jars");
        let Some(library) = compile_java_classes(&root, "lib", CROSS_JAR_LIBRARY, &[]) else {
            return;
        };
        let Some(app) = compile_java_classes(&root, "app", CROSS_JAR_MOD, &[&library]) else {
            return;
        };
        let library_jar = root.join("lib.jar");
        write_jar(&library_jar, &class_entries(&library));
        let app_jar = root.join("app.jar");
        let mut entries = class_entries(&app);
        entries.push(("fabric.mod.json".to_owned(), fabric_metadata("linked_app")));
        write_jar(&app_jar, &entries);

        let table = pack_class_table(&[library_jar.clone(), app_jar.clone()]);
        assert_eq!(table.jar_count(), 2, "both supplied jars are linked");
        assert!(table.contains_key("lib/Config"));
        assert!(table.contains_key("demo/Entry"));

        let unlinked = Arc::new(LinkedClassTable::default());
        let failure =
            match bootstrap_jar(&app_jar, ExecutionRole::DedicatedServer, &unlinked, false) {
                Ok(_) => panic!("the unlinked run must not see the library"),
                Err(failure) => failure,
            };
        assert!(
            failure.contains("IllegalStateException"),
            "the entrypoint must fail on the unlinked library: {failure}"
        );

        let run = bootstrap_jar(&app_jar, ExecutionRole::DedicatedServer, &table, false)
            .expect("the linked library must satisfy the entrypoint");
        assert_eq!(run.report.entrypoints, 1);
        assert_eq!(run.report.engine, JavaEngine::Native);
        assert!(run.game_hooks.is_none(), "the tier is off by default");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A jar bundled in `META-INF/jars` is on the pack class path too: the
    /// outer jar's bytecode resolves a library the outer jar only carries as a
    /// nested entry.
    #[test]
    fn a_nested_jar_joins_the_pack_class_table() {
        let root = scratch_root("nested-jar");
        let Some(library) = compile_java_classes(&root, "lib", CROSS_JAR_LIBRARY, &[]) else {
            return;
        };
        let Some(app) = compile_java_classes(&root, "app", CROSS_JAR_MOD, &[&library]) else {
            return;
        };
        let nested = root.join("lib.jar");
        write_jar(&nested, &class_entries(&library));
        let nested_bytes = std::fs::read(&nested).expect("the nested jar");

        let app_jar = root.join("app.jar");
        let mut entries = class_entries(&app);
        entries.push(("fabric.mod.json".to_owned(), fabric_metadata("bundle_app")));
        entries.push(("META-INF/jars/lib.jar".to_owned(), nested_bytes));
        write_jar(&app_jar, &entries);

        let table = pack_class_table(std::slice::from_ref(&app_jar));
        assert_eq!(table.jar_count(), 2, "the outer jar and its nested jar");
        assert!(
            table.contains_key("lib/Config"),
            "the nested library joins the class path"
        );
        let ids: Vec<_> = table.jar_ids().collect();
        assert!(ids.iter().any(|id| id.ends_with("!/META-INF/jars/lib.jar")));

        let unlinked = Arc::new(LinkedClassTable::default());
        assert!(
            bootstrap_jar(&app_jar, ExecutionRole::DedicatedServer, &unlinked, false).is_err(),
            "the outer jar alone does not carry the library"
        );
        bootstrap_jar(&app_jar, ExecutionRole::DedicatedServer, &table, false)
            .expect("the nested library must satisfy the entrypoint");
        let _ = std::fs::remove_dir_all(root);
    }

    /// The measured question this wiring asks: of a real corpus, how many jars
    /// does the pack class path move from a refused entrypoint to a successful
    /// native one, and how many game-class plans does the hook tier bind to a
    /// host site instead of refusing?
    ///
    /// ```sh
    /// HORIZON_JAVA_CORPUS_DIR=~/horizon-corpus/corpus-xl \
    ///   cargo test -p horizon-client --lib -- --ignored game_hook_tier_measures --nocapture
    /// ```
    #[test]
    #[ignore = "reads a real mod corpus outside the repository"]
    fn game_hook_tier_measures_the_needs_loader_set() {
        let Ok(dir) = std::env::var("HORIZON_JAVA_CORPUS_DIR") else {
            eprintln!("skipping: set HORIZON_JAVA_CORPUS_DIR");
            return;
        };
        let root = PathBuf::from(dir);
        let kind = std::env::var("HORIZON_JAVA_CORPUS_KIND").unwrap_or_else(|_| "server".into());
        let client = kind == "client";
        let side = if client {
            horizon_native_java::MixinSide::Client
        } else {
            horizon_native_java::MixinSide::Server
        };
        // The same entrypoint kinds `bootstrap_jar` tries for the role: a
        // Fabric jar commonly declares only `main`, which both roles run.
        let kinds = if client {
            ["client", "main", "neoforge"]
        } else {
            ["server", "main", "neoforge"]
        };
        let mut jars = std::fs::read_dir(&root)
            .expect("list the corpus")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|extension| extension == "jar"))
            .collect::<Vec<_>>();
        jars.sort();
        let table = pack_class_table(&jars);
        let empty = Arc::new(LinkedClassTable::default());
        let role = if client {
            ExecutionRole::Client
        } else {
            ExecutionRole::DedicatedServer
        };
        let mut considered = 0usize;
        let mut needs_loader = 0usize;
        let mut still_needs_loader = 0usize;
        let mut newly_runs = 0usize;
        let mut still_refused = 0usize;
        let mut lost = 0usize;
        let mut bound_jars = 0usize;
        let mut bound_runs = 0usize;
        for path in &jars {
            let Ok(jar) = NativeJar::from_path(path) else {
                continue;
            };
            if !kinds
                .iter()
                .any(|kind| !jar.discover_entrypoints(kind).is_empty())
            {
                continue;
            }
            considered += 1;
            let before = jar.plan_load_time_weave(side);
            let after = jar.plan_load_time_weave_with_game_hooks(side);
            let was = first_game_skip_reason(&before.skipped).is_some();
            let now = first_game_skip_reason(&after.skipped).is_some();
            if was {
                needs_loader += 1;
            }
            if now {
                still_needs_loader += 1;
            }
            // The linked arm enables both halves of this wiring: the pack's
            // class path and the game-class hook tier.
            let unlinked = bootstrap_jar(path, role, &empty, false);
            let linked = bootstrap_jar(path, role, &table, true);
            if let Ok(run) = &linked
                && let Some(hooks) = &run.game_hooks
            {
                bound_jars += 1;
                bound_runs += hooks.len();
            }
            match (unlinked, linked) {
                (Err(_), Ok(_)) => newly_runs += 1,
                (Ok(_), Ok(_)) => {}
                (Ok(_), Err(error)) => {
                    lost += 1;
                    eprintln!(
                        "{} ran alone but not linked: {error}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    );
                }
                (Err(_), Err(_)) => still_refused += 1,
            }
        }
        println!(
            "{considered} jar(s) with a {kind} entrypoint; {} of {considered} now start on the pack \
             class path that did not alone, {still_refused} still refuse, {lost} regressed; \
             {needs_loader} needed the loader by weave before, {still_needs_loader} still do, \
             {bound_jars} jar(s) bound {bound_runs} callback(s) at a host site; {} moved off the \
             weave set",
            newly_runs,
            needs_loader.saturating_sub(still_needs_loader)
        );
    }
}

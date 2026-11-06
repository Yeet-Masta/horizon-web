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
use super::execution_role::ExecutionRole;
use horizon_client::mod_content::ContentMod;
use horizon_mods::native::NativeDeclarations;
use std::{
    path::PathBuf,
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
/// entrypoints ran.
struct JavaJarRun {
    jar: horizon_native_java::NativeJar,
    host: horizon_native_java::StdHost,
    content: Vec<ContentMod>,
    report: JavaCompatibilityReport,
    declarations: NativeDeclarations,
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

fn run_one(path: &std::path::Path, role: ExecutionRole) -> Result<JavaJarRun, String> {
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
        jar,
        host,
        content,
        report,
        declarations,
    })
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
    run_java_mods_batch_with(paths, role, run_one)
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
        mut host,
        content,
        report,
        declarations,
    } = match outcome {
        Ok(run) => run,
        Err(failure) => {
            batch.failures.push(failure);
            return;
        }
    };
    let id = handler_role(role);
    let adopted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        super::java_payloads::adopt_host(id, &jar, &mut host, payload_declarations);
    }));
    // Dispatch keeps the hosts that declared payload channels
    // (`java_payloads::store_handler_host_as`); no other consumer needs the
    // parsed jar or its resources after its entrypoints ran, so do not hold
    // them for the process lifetime.
    drop(jar);
    drop(host);
    if adopted.is_err() {
        batch.failures.push(JavaJarFailure {
            jar: report.jar.clone(),
            reason: "panicked while its registrations were adopted".to_owned(),
        });
        return;
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
    let mut batch = JavaBatchOutcome::default();
    // The shared adoption step is serial, in path order: `adopt_host` mutates
    // the role's channel and handler maps, and a received frame answers from
    // the batch that came before it.
    let mut payload_declarations = Vec::new();
    bootstrap_into(paths, role, run, |outcome| {
        adopt_run(&mut batch, &mut payload_declarations, role, outcome);
    });
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
                    jar: horizon_native_java::NativeJar::default(),
                    host: horizon_native_java::StdHost::new(),
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
                jar: horizon_native_java::NativeJar::default(),
                host: horizon_native_java::StdHost::new(),
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
}

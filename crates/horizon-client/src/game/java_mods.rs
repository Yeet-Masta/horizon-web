//! Client-side native Java mod bootstrap.
use super::execution_role::ExecutionRole;
use horizon_client::mod_content::ContentMod;
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

fn run_one(
    path: &PathBuf,
    role: ExecutionRole,
) -> Result<(Option<ContentMod>, JavaCompatibilityReport), String> {
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
    let mut host = jar.seed_host(
        horizon_native_java::StdHost::new().with_environment_type(if role.is_client() {
            "CLIENT"
        } else {
            "SERVER"
        }),
    );
    for entry in &entries {
        jar.execute(&entry.class, entry.method.as_deref(), kind, &mut host)
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    // Current main has no native declaration-to-Content adapter yet. Preserve
    // the Java namespace in the content list; the registry adapter is the next
    // integration wave and can replace this empty declaration.
    let content = mod_id(&jar, path).map(|id| ContentMod {
        content: horizon_mods::Content::declared(&id),
        id,
    });
    let report = JavaCompatibilityReport {
        jar: path.clone(),
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
    Ok((content, report))
}

pub(super) fn run_java_mods(
    paths: &[PathBuf],
    role: ExecutionRole,
) -> Result<(Vec<ContentMod>, Vec<JavaCompatibilityReport>), String> {
    runtimes().lock().unwrap().clear();
    let mut content = Vec::new();
    let mut reports = Vec::new();
    for path in paths {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_one(path, role))) {
            Ok(Ok((c, r))) => {
                if let Some(c) = c {
                    content.push(c);
                }
                reports.push(r);
            }
            Ok(Err(e)) => log::warn!("Horizon: Java mod {e} skipped"),
            Err(_) => log::warn!(
                "Horizon: Java mod {} panicked and was skipped",
                path.display()
            ),
        }
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

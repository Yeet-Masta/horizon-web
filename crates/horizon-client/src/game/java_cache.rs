//! A bounded on-disk cache of one jar's native bootstrap result.
//!
//! The interpreter's work for a jar — parsing its classes, weaving its mixins
//! and running its entrypoints — depends on nothing but the jar's bytes, this
//! build's engine and the execution role. So a jar that started once can start
//! again from what it declared, without interpreting anything: the entry holds
//! the typed registration rows, the jar's own metadata rows and the report the
//! batch would have written, keyed by the jar's sha256 and length.
//!
//! The key is verified on every load, not inferred from the file name: a jar
//! whose bytes changed has another sha256, so it looks up another entry, and an
//! entry whose recorded hash or length does not match the file it was found
//! under is dropped. A jar that declared payload channels is never cached,
//! because those channels need the bootstrapped host, not just its rows.
//!
//! The cache lives under the game home (`java-content-cache/`), is bounded in
//! both bytes and entries, and is pruned after a batch by removing the least
//! recently used entries. `HORIZON_JAVA_CACHE=0` turns it off.
use crate::game::execution_role::ExecutionRole;
use horizon_native_java::{RegistryDeclaration, RegistryEntry};
use horizon_platform::{fs, time::SystemTime};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Layout of an entry, and the engine identity that produced it. Bump it when
/// the entry format changes; a differing schema is a miss, never a guess.
pub(super) const SCHEMA: u32 = 1;

/// Total bytes the cache may keep before the next prune removes entries.
const MAX_BYTES: u64 = 256 << 20;
/// Entries kept at all, so a pile of tiny jars cannot make the directory
/// itself the cost.
const MAX_ENTRIES: usize = 4096;
/// An entry is declaration rows, not data: anything this large is damage.
const MAX_ENTRY_BYTES: u64 = 8 << 20;

static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

/// A jar file's identity: what the cache is keyed by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Identity {
    pub(super) sha256: String,
    pub(super) size: u64,
}

/// One jar's cached bootstrap result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Entry {
    pub(super) schema: u32,
    pub(super) sha256: String,
    pub(super) size: u64,
    pub(super) role: String,
    pub(super) engine: String,
    pub(super) loader: Option<String>,
    pub(super) report_engine: String,
    pub(super) entrypoints: usize,
    pub(super) registrations: usize,
    pub(super) opaque: usize,
    pub(super) woven: usize,
    pub(super) skipped: usize,
    pub(super) first_skip_reason: Option<String>,
    pub(super) isolation_failure: Option<String>,
    /// The typed rows the host recorded, in registration order.
    pub(super) rows: Vec<Row>,
    /// The jar's own `META-INF/horizon-native.json` rows.
    pub(super) metadata_rows: Vec<Row>,
}

/// One typed registration as the cache stores it: the fields
/// [`RegistryDeclaration`] carries, without the interpreter's opaque value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Row {
    pub(super) registry: String,
    pub(super) id: String,
    pub(super) declaration: Declaration,
}

/// [`RegistryDeclaration`] with serde. Kept beside it rather than derived on it
/// because the declaration vocabulary is the interpreter's, not the cache's,
/// and an unknown variant must be a cache miss instead of a silent default.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Declaration {
    Block {
        template: String,
        properties: BTreeMap<String, String>,
        item: bool,
    },
    Item {
        template: String,
        properties: BTreeMap<String, String>,
    },
    MobEffect {
        category: String,
        color: u32,
    },
    Recipe {
        json: String,
    },
    Ingredient {
        json: String,
    },
    Custom {
        kind: String,
        properties: BTreeMap<String, String>,
    },
}
impl Declaration {
    fn from_declaration(declaration: &RegistryDeclaration) -> Self {
        match declaration {
            RegistryDeclaration::Block {
                template,
                properties,
                item,
            } => Self::Block {
                template: template.clone(),
                properties: properties.clone(),
                item: *item,
            },
            RegistryDeclaration::Item {
                template,
                properties,
            } => Self::Item {
                template: template.clone(),
                properties: properties.clone(),
            },
            RegistryDeclaration::MobEffect { category, color } => Self::MobEffect {
                category: category.clone(),
                color: *color,
            },
            RegistryDeclaration::Recipe { json } => Self::Recipe { json: json.clone() },
            RegistryDeclaration::Ingredient { json } => Self::Ingredient { json: json.clone() },
            RegistryDeclaration::Custom { kind, properties } => Self::Custom {
                kind: kind.clone(),
                properties: properties.clone(),
            },
        }
    }

    fn to_declaration(&self) -> RegistryDeclaration {
        match self {
            Self::Block {
                template,
                properties,
                item,
            } => RegistryDeclaration::Block {
                template: template.clone(),
                properties: properties.clone(),
                item: *item,
            },
            Self::Item {
                template,
                properties,
            } => RegistryDeclaration::Item {
                template: template.clone(),
                properties: properties.clone(),
            },
            Self::MobEffect { category, color } => RegistryDeclaration::MobEffect {
                category: category.clone(),
                color: *color,
            },
            Self::Recipe { json } => RegistryDeclaration::Recipe { json: json.clone() },
            Self::Ingredient { json } => RegistryDeclaration::Ingredient { json: json.clone() },
            Self::Custom { kind, properties } => RegistryDeclaration::Custom {
                kind: kind.clone(),
                properties: properties.clone(),
            },
        }
    }
}

impl Row {
    /// The cache row for one host registration, or `None` when it is opaque and
    /// declares nothing (which is most of a real jar's registrations).
    pub(super) fn from_entry(entry: &RegistryEntry) -> Option<Self> {
        Some(Self {
            registry: entry.registry.clone(),
            id: entry.id.clone(),
            declaration: Declaration::from_declaration(entry.declaration.as_ref()?),
        })
    }

    /// The typed row the declaration converter takes. The opaque value is
    /// reconstructed deterministically, exactly as [`RegistryEntry::typed`]
    /// builds it; the converter only reads the declaration.
    pub(super) fn to_entry(&self) -> RegistryEntry {
        RegistryEntry::typed(
            self.registry.clone(),
            self.id.clone(),
            self.declaration.to_declaration(),
        )
    }
}

impl Entry {
    /// Whether this entry was produced by the current build and role, and for
    /// the file it was found under.
    fn matches(&self, identity: &Identity, role: ExecutionRole, engine: &str) -> bool {
        self.schema == SCHEMA
            && self.sha256 == identity.sha256
            && self.size == identity.size
            && self.role == role_key(role)
            && self.engine == engine
    }
}

/// The cache is on unless `HORIZON_JAVA_CACHE` says otherwise.
pub(super) fn enabled() -> bool {
    !matches!(
        horizon_platform::env::var("HORIZON_JAVA_CACHE").as_deref(),
        Ok("0" | "false" | "off")
    )
}

/// The engine identity a cache entry belongs to: this build's interpreter. A
/// release names its commit; a local build falls back to the executable's own
/// modification time, as the far-terrain cache does, so a rebuilt engine starts
/// afresh instead of replaying another engine's declarations.
/// `HORIZON_JAVA_CACHE_ENGINE` overrides it for an A/B run.
pub(super) fn engine_key() -> String {
    if let Ok(engine) = horizon_platform::env::var("HORIZON_JAVA_CACHE_ENGINE")
        && !engine.is_empty()
    {
        return engine;
    }
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    SCHEMA.hash(&mut hasher);
    env!("CARGO_PKG_VERSION").hash(&mut hasher);
    match option_env!("HORIZON_COMMIT") {
        Some(commit) => commit.hash(&mut hasher),
        None => horizon_platform::env::current_exe()
            .and_then(|exe| fs::metadata(exe)?.modified())
            .ok()
            .hash(&mut hasher),
    }
    format!("native-{:016x}", hasher.finish())
}

/// How one execution role is named in the key. The same jar can declare
/// different rows for a client and a server, so the role is part of the key.
pub(super) fn role_key(role: ExecutionRole) -> &'static str {
    match role {
        ExecutionRole::Client => "client",
        ExecutionRole::IntegratedServer => "integrated-server",
        ExecutionRole::DedicatedServer => "dedicated-server",
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0xf)] as char);
    }
    out
}

fn is_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The jar's sha256 and length. `None` when the file cannot be read, which
/// leaves the jar to the interpreter's own error path.
pub(super) fn identity(path: &Path) -> Option<Identity> {
    let mut file = fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 << 10];
    let mut size = 0u64;
    loop {
        let read = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Some(Identity {
        sha256: hex(&hasher.finalize()),
        size,
    })
}

/// The cache's home. `HORIZON_JAVA_CACHE` or `--home` move it with the game
/// home, so a scratch run and a player's install never share entries.
fn root() -> PathBuf {
    crate::logging::game_home().join("java-content-cache")
}

fn entry_path(root: &Path, sha256: &str, role: ExecutionRole) -> Option<PathBuf> {
    is_hex(sha256).then(|| root.join(format!("{sha256}-{}.json", role_key(role))))
}

/// The jar's identity and the entry already stored for it, when the cache is
/// on. `None` as a whole means the cache does not apply to this jar at all (off,
/// or unreadable); `Some((identity, None))` means a miss to bootstrap and then
/// store. A hit counts for the batch's log line.
pub(super) fn lookup(path: &Path, role: ExecutionRole) -> Option<(Identity, Option<Entry>)> {
    if !enabled() {
        return None;
    }
    let identity = identity(path)?;
    match load_in(&root(), &identity, role, &engine_key()) {
        Some(entry) => {
            HITS.fetch_add(1, Ordering::Relaxed);
            Some((identity, Some(entry)))
        }
        None => {
            MISSES.fetch_add(1, Ordering::Relaxed);
            Some((identity, None))
        }
    }
}

/// Stores one jar's result for the next start. The entry carries the identity
/// it was written for, and `write_in` refuses one that is not a sha256.
pub(super) fn remember(entry: &Entry) {
    if enabled() {
        write_in(&root(), entry);
    }
}

/// Hits and misses since the process started, for the batch's summary line.
pub(super) fn stats() -> (u64, u64) {
    (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
}

/// Reads the entry for an identity. A file that does not parse, or that does
/// not describe this jar, this role or this engine, is removed and counted as
/// a miss: it is never used.
fn load_in(root: &Path, identity: &Identity, role: ExecutionRole, engine: &str) -> Option<Entry> {
    let path = entry_path(root, &identity.sha256, role)?;
    let metadata = fs::metadata(&path).ok()?;
    if metadata.len() > MAX_ENTRY_BYTES {
        let _ = fs::remove_file(&path);
        return None;
    }
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return None,
    };
    let entry: Entry = match serde_json::from_slice(&bytes) {
        Ok(entry) => entry,
        Err(_) => {
            let _ = fs::remove_file(&path);
            return None;
        }
    };
    if !entry.matches(identity, role, engine) {
        let _ = fs::remove_file(&path);
        return None;
    }
    // A hit counts as a use for the size-bounded prune.
    if let Ok(file) = fs::OpenOptions::new().write(true).open(&path) {
        let _ = file.set_modified(SystemTime::now());
    }
    Some(entry)
}

/// Writes one entry through a temporary file, so a crash mid-write leaves no
/// half an entry that a later start would read.
fn write_in(root: &Path, entry: &Entry) {
    if !is_hex(&entry.sha256) {
        return;
    }
    let Ok(bytes) = serde_json::to_vec(entry) else {
        return;
    };
    if bytes.len() as u64 > MAX_ENTRY_BYTES {
        return;
    }
    let Some(role) = role_from_key(&entry.role) else {
        return;
    };
    let Some(path) = entry_path(root, &entry.sha256, role) else {
        return;
    };
    if fs::create_dir_all(root).is_err() {
        return;
    }
    let temporary = path.with_extension(format!("json.{}.tmp", horizon_platform::process::id()));
    if fs::write(&temporary, &bytes).is_err() || fs::rename(&temporary, &path).is_err() {
        let _ = fs::remove_file(&temporary);
    }
}

fn role_from_key(key: &str) -> Option<ExecutionRole> {
    match key {
        "client" => Some(ExecutionRole::Client),
        "integrated-server" => Some(ExecutionRole::IntegratedServer),
        "dedicated-server" => Some(ExecutionRole::DedicatedServer),
        _ => None,
    }
}

/// Removes the least recently used entries until the cache is inside both
/// bounds. Called once after a batch, so a pack that grows a new jar does not
/// grow the directory without limit.
pub(super) fn prune() {
    if enabled() {
        prune_in(&root());
    }
}

fn prune_in(root: &Path) {
    let Ok(dir) = fs::read_dir(root) else {
        return;
    };
    let mut entries: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    for item in dir.flatten() {
        let path = item.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(metadata) = item.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let used = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        entries.push((used, metadata.len(), path));
    }
    let mut total: u64 = entries.iter().map(|(_, size, _)| *size).sum();
    let mut count = entries.len();
    if total <= MAX_BYTES && count <= MAX_ENTRIES {
        return;
    }
    entries.sort_by_key(|entry| entry.0);
    for (_, size, path) in entries {
        if total <= MAX_BYTES && count <= MAX_ENTRIES {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
            count -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let root = horizon_platform::env::temp_dir()
            .join(format!("horizon-java-cache-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("a scratch cache root");
        root
    }

    fn entry(identity: &Identity, role: ExecutionRole, engine: &str) -> Entry {
        Entry {
            schema: SCHEMA,
            sha256: identity.sha256.clone(),
            size: identity.size,
            role: role_key(role).to_owned(),
            engine: engine.to_owned(),
            loader: Some("fabric".into()),
            report_engine: "native".into(),
            entrypoints: 1,
            registrations: 1,
            opaque: 0,
            woven: 0,
            skipped: 0,
            first_skip_reason: None,
            isolation_failure: None,
            rows: vec![Row {
                registry: "minecraft:block".into(),
                id: "fixture:ruby_block".into(),
                declaration: Declaration::Block {
                    template: "minecraft:stone".into(),
                    properties: BTreeMap::from([("hardness".into(), "1.5".into())]),
                    item: true,
                },
            }],
            metadata_rows: Vec::new(),
        }
    }

    #[test]
    fn a_changed_jar_has_another_key_and_misses() {
        let root = scratch("changed-jar");
        let jar = root.join("mod.jar");
        fs::write(&jar, b"PK\x03\x04 first build").unwrap();
        let first = identity(&jar).expect("the jar is readable");
        write_in(
            &root,
            &entry(&first, ExecutionRole::DedicatedServer, "engine"),
        );

        let before = identity(&jar).unwrap();
        assert!(load_in(&root, &before, ExecutionRole::DedicatedServer, "engine").is_some());

        // A byte more: another sha256, so another key, and nothing to hit.
        fs::write(&jar, b"PK\x03\x04 first build, patched").unwrap();
        let after = identity(&jar).unwrap();
        assert_ne!(first.sha256, after.sha256);
        assert!(
            load_in(&root, &after, ExecutionRole::DedicatedServer, "engine").is_none(),
            "a tampered jar must not be served the entry of the jar it replaced"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_entry_that_names_another_hash_or_size_is_dropped() {
        let root = scratch("stale-entry");
        let jar = root.join("mod.jar");
        fs::write(&jar, b"jar bytes").unwrap();
        let real = identity(&jar).unwrap();
        // An entry copied under another jar's name still has to agree with the
        // file it was found under, not with its own path.
        let mut forged = entry(&real, ExecutionRole::Client, "engine");
        forged.sha256 = "0".repeat(64);
        let path = entry_path(&root, &real.sha256, ExecutionRole::Client).unwrap();
        fs::write(&path, serde_json::to_vec(&forged).unwrap()).unwrap();
        assert!(
            load_in(&root, &real, ExecutionRole::Client, "engine").is_none(),
            "a row's recorded sha256 is verified, not trusted"
        );

        write_in(&root, &entry(&real, ExecutionRole::Client, "engine"));
        assert!(load_in(&root, &real, ExecutionRole::Client, "engine").is_some());
        let mut wrong_size = entry(&real, ExecutionRole::Client, "engine");
        wrong_size.size += 1;
        write_in(&root, &wrong_size);
        assert!(
            load_in(&root, &real, ExecutionRole::Client, "engine").is_none(),
            "a length that does not match the file is a miss"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn another_role_or_engine_does_not_answer() {
        let root = scratch("role-engine");
        let jar = root.join("mod.jar");
        fs::write(&jar, b"jar bytes").unwrap();
        let real = identity(&jar).unwrap();
        write_in(
            &root,
            &entry(&real, ExecutionRole::DedicatedServer, "engine-a"),
        );
        assert!(load_in(&root, &real, ExecutionRole::DedicatedServer, "engine-a").is_some());
        assert!(load_in(&root, &real, ExecutionRole::Client, "engine-a").is_none());
        assert!(load_in(&root, &real, ExecutionRole::DedicatedServer, "engine-b").is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_corrupt_entry_is_removed_instead_of_read() {
        let root = scratch("corrupt");
        let jar = root.join("mod.jar");
        fs::write(&jar, b"jar bytes").unwrap();
        let real = identity(&jar).unwrap();
        let path = entry_path(&root, &real.sha256, ExecutionRole::Client).unwrap();
        fs::write(&path, b"{ not an entry").unwrap();
        assert!(load_in(&root, &real, ExecutionRole::Client, "engine").is_none());
        assert!(!path.exists(), "a corrupt entry is deleted");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn pruning_keeps_the_cache_inside_both_bounds() {
        let root = scratch("prune");
        // More entries than the count bound allows, oldest first, each with a
        // distinct mtime so the order is the least recently used one.
        let now = SystemTime::now();
        for index in 0..(MAX_ENTRIES as u64 + 3) {
            let sha = format!("{index:064x}");
            let path = entry_path(&root, &sha, ExecutionRole::Client).unwrap();
            fs::write(&path, b"{}").unwrap();
            if let Ok(file) = fs::OpenOptions::new().write(true).open(&path) {
                let _ = file.set_modified(
                    now - horizon_platform::time::Duration::from_secs(10_000 - index),
                );
            }
        }
        prune_in(&root);
        let left = fs::read_dir(&root).unwrap().count();
        assert!(left <= MAX_ENTRIES, "{left} entries left");
        // The newest entry survived; the oldest did not.
        assert!(
            entry_path(
                &root,
                &format!("{:064x}", MAX_ENTRIES as u64 + 2),
                ExecutionRole::Client
            )
            .unwrap()
            .exists()
        );
        assert!(
            !entry_path(&root, &format!("{:064x}", 0), ExecutionRole::Client)
                .unwrap()
                .exists()
        );
        let _ = fs::remove_dir_all(&root);
    }
}

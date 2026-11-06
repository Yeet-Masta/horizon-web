//! Java mods (Fabric, NeoForge, Forge) from the browser (docs/mod-browser.md). Horizon doesn't
//! run their code; it ports what they carry (blocks, items, recipes, loot, tags, worldgen) with
//! `horizon-port-pack`. Before a mod is offered for install its jar is downloaded (checked as
//! every download is), carried over and its classes read (`horizon_port_pack::check`): the
//! verdict (Works, Partly, Doesn't run) and the lists of what ports and what doesn't come from
//! that, and are kept per file so a jar is checked once.
//!
//! Installing copies the carried-over folder mod into `horizon-mods/<mod id>/` beside
//! `options.txt` and turns it on in `horizon-mods.txt`, where the game's Options > Mods reads it.
use super::{installed::Entry, pick, FileRef, Kind, Project, Source, Support, Version};
use crate::i18n::{self, Language};
use horizon_port_pack::check::{self, Check, Verdict};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Where the checks are kept, under the game folder. Its name changes when the check finds
/// more, so older answers are checked again.
const CHECKS: &str = "mod-browser/checks-1";
/// The game's mod folder and on-list, beside `options.txt` (`horizon_client::mod_list`).
pub const MODS_DIR: &str = "horizon-mods";
const MODS_LIST: &str = "horizon-mods.txt";
/// How deep required mods are followed.
const MAX_DEPTH: usize = 3;

/// The loaders whose jars port-pack reads.
pub const LOADERS: [&str; 4] = ["neoforge", "fabric", "forge", "quilt"];

/// A message or path on one line, cut to a player-sized length.
fn bounded(text: &str) -> String {
    const MAX_DETAIL: usize = 240;
    let mut detail = text.replace(['\n', '\r'], " ");
    if detail.chars().count() > MAX_DETAIL {
        detail = detail.chars().take(MAX_DETAIL - 1).collect();
        detail.push('…');
    }
    detail
}

/// The schema of the game's `java-runtime.json` (crates/horizon-client/src/game/java_runtime_file.rs).
/// A newer schema is ignored rather than guessed at.
const RUNTIME_SCHEMA: u32 = 1;
const RUNTIME_MAX_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeReport {
    schema_version: u32,
    jars: Vec<RuntimeJar>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RuntimeJar {
    file: String,
    #[serde(default)]
    loader: String,
    #[serde(default)]
    engine: String,
    #[serde(default)]
    woven: usize,
    #[serde(default)]
    skipped: usize,
    #[serde(default)]
    first_skip_reason: Option<String>,
    #[serde(default)]
    registrations: usize,
    #[serde(default)]
    opaque: usize,
    /// Later games report declared channels; older ones leave them out.
    #[serde(default)]
    payload_channels: Vec<String>,
    #[serde(default)]
    isolation_failure: Option<String>,
}

/// One installed jar's runtime lines, and how relevant they are to the panel: whether the
/// runtime failed to isolate it, and how many classes it skipped.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuntimeLines {
    pub lines: Vec<String>,
    pub failed: bool,
    pub skipped: usize,
}

/// Reads the game's bounded runtime report, if one was written for this game folder. A file
/// larger than the game writes, or one of another schema, is ignored.
pub fn runtime_report(home: &Path) -> Option<RuntimeReport> {
    let bytes = fs::read(home.join("java-runtime.json")).ok()?;
    if bytes.len() > RUNTIME_MAX_BYTES {
        return None;
    }
    let report: RuntimeReport = serde_json::from_slice(&bytes).ok()?;
    (report.schema_version == RUNTIME_SCHEMA).then_some(report)
}

/// Bounded runtime lines for one installed jar, matched by its file name. Every reported
/// string is cut to [`bounded`], so a hostile report can't grow the panel.
pub fn runtime_lines_in(
    language: Language,
    report: &RuntimeReport,
    file_name: &str,
) -> Option<RuntimeLines> {
    // An installed Java mod is a folder named for its mod id, while the runtime reports the
    // jar's file name: match the file name, the whole path, or the name without `.jar`.
    let jar = report.jars.iter().find(|jar| {
        let path = Path::new(&jar.file);
        path.file_name().is_some_and(|name| name == file_name)
            || path.file_stem().is_some_and(|stem| stem == file_name)
            || jar.file == file_name
    })?;
    let woven = jar.woven.to_string();
    let skipped = jar.skipped.to_string();
    let registrations = jar.registrations.to_string();
    let opaque = jar.opaque.to_string();
    let payloads = jar.payload_channels.len().to_string();
    let mut lines = vec![say(
        language,
        "Runtime: {engine} · loader: {loader} · woven: {woven} · skipped: {skipped} · registrations: {registrations} · opaque: {opaque} · payloads: {payloads}",
        &[
            ("engine", &bounded(&jar.engine)),
            ("loader", &bounded(&jar.loader)),
            ("woven", &woven),
            ("skipped", &skipped),
            ("registrations", &registrations),
            ("opaque", &opaque),
            ("payloads", &payloads),
        ],
    )];
    if let Some(reason) = jar.first_skip_reason.as_deref() {
        lines.push(say(
            language,
            "Runtime skipped: {reason}",
            &[("reason", &bounded(reason))],
        ));
    }
    if let Some(error) = jar.isolation_failure.as_deref() {
        lines.push(say(
            language,
            "Runtime isolation: {error}",
            &[("error", &bounded(error))],
        ));
    }
    Some(RuntimeLines {
        lines,
        failed: jar.isolation_failure.is_some(),
        skipped: jar.skipped,
    })
}

/// One summary line when the report has no jar the page listed.
pub fn runtime_summary_in(language: Language, report: &RuntimeReport) -> String {
    let count = report.jars.len().to_string();
    say(
        language,
        "Runtime: {count} jar(s) reported",
        &[("count", &count)],
    )
}

/// `key` as `language` says it, its `{name}` holes filled with `args`.
fn say(language: Language, key: &str, args: &[(&str, &str)]) -> String {
    let mut out = i18n::tr_in(language, key).to_string();
    for (name, value) in args {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

/// How well a checked mod runs.
pub fn support_of(check: &Check) -> Support {
    match check.verdict {
        Verdict::Works => Support::Works,
        Verdict::Partly => Support::Partial(check.summary.clone()),
        Verdict::No => Support::No(check.summary.clone()),
    }
}

fn checks(home: &Path) -> PathBuf {
    home.join(CHECKS)
}

/// The folder a file's check is kept in, by its checksum.
fn checked_dir(home: &Path, file: &FileRef) -> Option<PathBuf> {
    let (_, hash) = file.checksum()?;
    checked_dir_of(home, hash)
}

/// The folder for a checksum's hex digits. Only hex survives and the name is cut to 40 digits,
/// so a hostile value can't leave the checks folder.
fn checked_dir_of(home: &Path, hash: &str) -> Option<PathBuf> {
    let hash: String = hash
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(40)
        .collect();
    (hash.len() >= 32).then(|| checks(home).join(hash))
}

/// The folder for an installed entry's `ALGO:hex` checksum (`installed::Entry::checksum`).
fn checked_dir_for_entry(home: &Path, checksum: &str) -> Option<PathBuf> {
    checked_dir_of(
        home,
        checksum.split_once(':').map_or(checksum, |(_, hex)| hex),
    )
}

fn read_check(dir: &Path) -> Option<Check> {
    serde_json::from_str(&fs::read_to_string(dir.join("check.json")).ok()?).ok()
}

/// The check of `file` if it was made before.
pub fn cached(home: &Path, file: &FileRef) -> Option<Check> {
    read_check(&checked_dir(home, file)?)
}

/// The cached check of an installed mod, by the `ALGO:hex` checksum its entry keeps.
pub fn cached_for_entry(home: &Path, checksum: &str) -> Option<Check> {
    read_check(&checked_dir_for_entry(home, checksum)?)
}

/// Forgets one installed mod's cached check (and any half-written copy of it) so the next
/// check of that jar reads it again. Only that checksum's folder goes; every other mod's
/// check, and the `.downloads` scratch folder, stay. Returns whether anything was removed.
pub fn clear_check(home: &Path, checksum: &str) -> Result<bool, String> {
    let dir = checked_dir_for_entry(home, checksum)
        .ok_or_else(|| format!("{checksum} isn't a checksum a check is kept under."))?;
    let mut removed = false;
    for path in [dir.clone(), dir.with_extension("part")] {
        match fs::remove_dir_all(&path) {
            Ok(()) => removed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    Ok(removed)
}

/// Checks `file`: the cached answer, else its jar downloaded, carried over and read. The jar
/// itself isn't kept; the carry is, when something of it ports, for the install to copy.
pub fn check_file(
    home: &Path,
    source: &dyn Source,
    file: &FileRef,
    progress: &dyn Fn(f32),
) -> Result<Check, String> {
    let dir = checked_dir(home, file).ok_or("Its source gives no checksum.")?;
    if let Some(c) = read_check(&dir) {
        return Ok(c);
    }
    let jar = source.fetch(file, &checks(home).join(".downloads"), progress)?;
    let part = dir.with_extension("part");
    let _ = fs::remove_dir_all(&part);
    let result = check::check(&jar, &part);
    let _ = fs::remove_file(&jar);
    let c = result.inspect_err(|_| {
        let _ = fs::remove_dir_all(&part);
    })?;
    if c.verdict == Verdict::No {
        for d in ["carry", "outside"] {
            let _ = fs::remove_dir_all(part.join(d));
        }
    }
    let _ = fs::remove_dir_all(&dir);
    fs::rename(&part, &dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(c)
}

/// The version of `project` to install and its check.
pub fn check_project(
    home: &Path,
    source: &dyn Source,
    project: &Project,
    everything: bool,
    progress: &dyn Fn(f32),
) -> Result<(Version, Check), String> {
    let versions = source.versions(project)?;
    let version = pick(Kind::Mod, &versions, everything)
        .cloned()
        .ok_or_else(|| match versions.iter().find_map(|v| v.blocked.clone()) {
            Some(why) => format!("{} can't be downloaded from here: {why}", project.title),
            None => format!(
                "{} has no Fabric, NeoForge or Forge version Horizon can port.",
                project.title
            ),
        })?;
    let file = version.file().expect("pick keeps versions with a file");
    let check = check_file(home, source, file, progress)?;
    Ok((version, check))
}

/// Whether mod `id` is on in `horizon-mods.txt`.
pub fn is_enabled(home: &Path, id: &str) -> bool {
    let key = format!("mod.{id}");
    fs::read_to_string(home.join(MODS_LIST))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once(':'))
        .any(|(k, v)| k.trim() == key && v.trim() == "true")
}

/// Turns mod `id` on (or off) in `horizon-mods.txt`, keeping the other lines.
pub fn set_enabled(home: &Path, id: &str, on: bool) -> Result<(), String> {
    let _lock = super::lock_home(home);
    let path = home.join(MODS_LIST);
    let text = fs::read_to_string(&path).unwrap_or_default();
    let key = format!("mod.{id}");
    let mut lines: Vec<String> = text
        .lines()
        .filter(|l| l.split_once(':').is_none_or(|(k, _)| k.trim() != key))
        .map(str::to_owned)
        .collect();
    if on {
        lines.push(format!("{key}:true"));
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    fs::write(&path, out).map_err(|e| format!("{}: {e}", path.display()))
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    for entry in fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            fs::copy(&src, &dst).map_err(|e| format!("{}: {e}", dst.display()))?;
        }
    }
    Ok(())
}

/// Installs the Java mod `project`: checked, carried over into `horizon-mods/<id>/` and turned
/// on, with the mods it requires that port too. A required mod with nothing to port (a library)
/// is listed in what was skipped. Returns the entries installed, the asked-for one last.
pub fn install(
    source: &dyn Source,
    project: &Project,
    everything: bool,
    home: &Path,
    progress: &dyn Fn(&str, f32),
) -> Result<Vec<Entry>, String> {
    let _lock = super::lock_home(home);
    let job = Job {
        source,
        everything,
        home,
        progress,
    };
    let mut done = Vec::new();
    job.add(project, false, 0, &mut done)?;
    Ok(done)
}

/// One install: where from, where to, and how it reports.
struct Job<'a> {
    source: &'a dyn Source,
    everything: bool,
    home: &'a Path,
    progress: &'a dyn Fn(&str, f32),
}

impl Job<'_> {
    fn add(
        &self,
        project: &Project,
        dependency: bool,
        depth: usize,
        done: &mut Vec<Entry>,
    ) -> Result<(), String> {
        let Job {
            source,
            everything,
            home,
            progress,
        } = *self;
        if done.iter().any(|e| e.project == project.id) {
            return Ok(());
        }
        let (version, check) = check_project(home, source, project, everything, &|f| {
            progress(&format!("Checking {}", project.title), f)
        })?;
        if check.verdict == Verdict::No {
            return Err(format!(
                "{} doesn't run in Horizon: {}",
                project.title, check.summary
            ));
        }
        let mut skipped = check.skipped.clone();
        for id in &version.required {
            let Ok(needed) = source.project(id) else {
                continue;
            };
            if needed.kind != Kind::Mod || depth >= MAX_DEPTH {
                continue;
            }
            match self.add(&needed, true, depth + 1, done) {
                Ok(()) => {}
                Err(_) if dependency => {}
                Err(why) => skipped.push(format!("needs {}: {why}", needed.title)),
            }
        }
        let file = version.file().expect("pick keeps versions with a file");
        let carry = checked_dir(home, file)
            .expect("checked above")
            .join("carry");
        let relative = format!("{MODS_DIR}/{}", check.mod_id);
        let mut installed = super::installed::Installed::load(home);
        let ours = |e: &Entry| e.source == project.source && e.project == project.id;
        if let Some(other) = installed.entries.iter().find(|e| {
            e.kind == Kind::Mod
                && !ours(e)
                && (e.file == relative || e.namespaces.iter().any(|n| check.namespaces.contains(n)))
        }) {
            return Err(format!(
                "{} shares its id or namespace with {}, installed already: remove one first.",
                project.title, other.title
            ));
        }
        let target = home.join(&relative);
        let old = installed.entries.iter().find(|e| ours(e)).cloned();
        if target.exists() && old.as_ref().is_none_or(|o| o.file != relative) {
            return Err(format!(
                "{relative} is already there and wasn't installed by the browser: move it away first."
            ));
        }
        progress(&format!("Installing {}", project.title), 1.);
        let part = home.join(MODS_DIR).join(format!(".{}.part", check.mod_id));
        let _ = fs::remove_dir_all(&part);
        copy_dir(&carry, &part).inspect_err(|_| {
            let _ = fs::remove_dir_all(&part);
        })?;
        if let Some(old) = &old {
            if let Some(dir) = old.path(home) {
                let _ = fs::remove_dir_all(dir);
            }
            if old.file != relative {
                let _ = set_enabled(home, old.file.trim_start_matches("horizon-mods/"), false);
            }
        }
        fs::rename(&part, &target).map_err(|e| format!("{}: {e}", target.display()))?;
        set_enabled(home, &check.mod_id, true)?;
        let (algo, hash) = file.checksum().expect("checked files have one");
        let entry = Entry {
            source: project.source,
            project: project.id.clone(),
            slug: project.slug.clone(),
            title: project.title.clone(),
            kind: Kind::Mod,
            version: version.id.clone(),
            number: version.number.clone(),
            file: relative,
            checksum: format!("{algo}:{hash}"),
            page: project.page.clone(),
            dependency: dependency && old.as_ref().is_none_or(|o| o.dependency),
            verdict: Some(check.verdict.label().to_owned()),
            skipped,
            namespaces: check.namespaces.clone(),
        };
        installed.entries.retain(|e| !ours(e));
        installed.entries.push(entry.clone());
        installed.save(home)?;
        done.push(entry);
        Ok(())
    }
}

/// Removes an installed mod's folder and turns it off.
pub fn remove(home: &Path, entry: &Entry) -> Result<(), String> {
    let _lock = super::lock_home(home);
    let Some(id) = entry.file.strip_prefix("horizon-mods/") else {
        return Err(format!("{} isn't in {MODS_DIR}/.", entry.file));
    };
    match entry.path(home).map_or(Ok(()), fs::remove_dir_all) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", entry.file)),
    }
    set_enabled(home, id, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_on_list_keeps_other_lines() {
        let home = std::env::temp_dir().join(format!("horizon-java-mods-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join(MODS_LIST),
            "mod.coords_hud:true\nmod.ruby:false\n",
        )
        .unwrap();
        set_enabled(&home, "ruby", true).unwrap();
        set_enabled(&home, "gem", true).unwrap();
        assert_eq!(
            fs::read_to_string(home.join(MODS_LIST)).unwrap(),
            "mod.coords_hud:true\nmod.ruby:true\nmod.gem:true\n"
        );
        set_enabled(&home, "ruby", false).unwrap();
        set_enabled(&home, "gem", false).unwrap();
        assert_eq!(
            fs::read_to_string(home.join(MODS_LIST)).unwrap(),
            "mod.coords_hud:true\n"
        );
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn checks_are_kept_by_checksum() {
        let home = Path::new("/h");
        let file = |sha512: Option<&str>, sha1: Option<&str>| FileRef {
            url: String::new(),
            name: "a.jar".into(),
            size: 1,
            sha512: sha512.map(str::to_owned),
            sha256: None,
            sha1: sha1.map(str::to_owned),
            primary: true,
        };
        let long = "ab".repeat(64);
        assert_eq!(
            checked_dir(home, &file(Some(&long), None)).unwrap(),
            home.join(CHECKS).join("ab".repeat(20))
        );
        assert!(checked_dir(home, &file(None, Some("../../etc"))).is_none());
        assert!(checked_dir(home, &file(None, None)).is_none());
    }

    fn demo_check() -> Check {
        Check {
            mod_id: "demo".into(),
            name: "Demo".into(),
            version: "1".into(),
            loader: "fabric".into(),
            license: "MIT".into(),
            verdict: Verdict::Works,
            summary: String::new(),
            ports: Vec::new(),
            skipped: Vec::new(),
            depends: Vec::new(),
            namespaces: Vec::new(),
            code: Default::default(),
        }
    }

    /// The runtime report is read from the game folder, matched by a jar's file name, and its
    /// strings are cut to one bounded line. A newer schema or an oversized file is ignored.
    #[test]
    fn runtime_report_is_read_bounded_and_matched_by_file_name() {
        let home = std::env::temp_dir().join(format!("horizon-runtime-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        let report = serde_json::json!({
            "schema_version": 1,
            "jars": [{
                "file": "mods/demo.jar", "loader": "fabric", "engine": "native",
                "woven": 2, "skipped": 1, "first_skip_reason": "bad target",
                "registrations": 4, "opaque": 1, "payload_channels": ["play"],
                "isolation_failure": null
            }, {
                "file": "other.jar", "loader": "forge", "engine": "native",
                "woven": 0, "skipped": 0, "first_skip_reason": null,
                "registrations": 0, "opaque": 0, "payload_channels": [],
                "isolation_failure": "class loader\nescaped"
            }]
        });
        fs::write(
            home.join("java-runtime.json"),
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
        let read = runtime_report(&home).unwrap();
        let lines = runtime_lines_in(Language::English, &read, "demo.jar").unwrap();
        assert!(lines.lines[0].contains("native"));
        assert!(lines.lines[0].contains("payloads: 1"));
        assert!(lines.lines[1].contains("bad target"));
        assert_eq!((lines.failed, lines.skipped), (false, 1));
        // An installed mod's entry names its folder, not the jar inside it.
        assert!(runtime_lines_in(Language::English, &read, "demo").is_some());
        assert!(runtime_lines_in(Language::English, &read, "missing.jar").is_none());

        let failed = runtime_lines_in(Language::English, &read, "other.jar").unwrap();
        assert!(failed.failed);
        assert!(!failed.lines.last().unwrap().contains('\n'));
        assert_eq!(
            runtime_summary_in(Language::English, &read),
            "Runtime: 2 jar(s) reported"
        );

        fs::write(
            home.join("java-runtime.json"),
            serde_json::json!({"schema_version": 2, "jars": []}).to_string(),
        )
        .unwrap();
        assert!(runtime_report(&home).is_none());
        fs::write(
            home.join("java-runtime.json"),
            "x".repeat(RUNTIME_MAX_BYTES + 1),
        )
        .unwrap();
        assert!(runtime_report(&home).is_none());
        fs::remove_dir_all(&home).unwrap();
    }

    /// Clearing one mod's check leaves the others (and the download scratch folder) alone, and
    /// the cache still answers for what remains.
    #[test]
    fn clearing_a_check_keeps_the_others() {
        let home = std::env::temp_dir().join(format!("horizon-java-clear-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let (a, b) = ("ab".repeat(64), "cd".repeat(64));
        let check = demo_check();
        for hex in [&a, &b] {
            let dir = checked_dir_of(&home, hex).unwrap();
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("check.json"), serde_json::to_vec(&check).unwrap()).unwrap();
        }
        fs::create_dir_all(checks(&home).join(".downloads")).unwrap();
        let (key_a, key_b) = (format!("SHA-512:{a}"), format!("SHA-512:{b}"));
        assert_eq!(cached_for_entry(&home, &key_a), Some(check.clone()));

        assert_eq!(clear_check(&home, &key_a), Ok(true));
        assert_eq!(cached_for_entry(&home, &key_a), None);
        assert_eq!(cached_for_entry(&home, &key_b), Some(check));
        assert!(checks(&home).join(".downloads").exists());
        // Already gone: nothing to remove, and not an error.
        assert_eq!(clear_check(&home, &key_a), Ok(false));
        // A value that isn't a checksum never reaches the file system.
        assert!(clear_check(&home, "SHA-512:../..").is_err());
        assert!(checks(&home).exists());
        fs::remove_dir_all(&home).unwrap();
    }
}

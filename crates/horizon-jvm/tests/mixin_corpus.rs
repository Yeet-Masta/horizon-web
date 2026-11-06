//! Gated check that real, mixin-heavy Fabric mods from the local corpus apply
//! their mixins during `--launch-game`.
//!
//! This pins the loader seams that make a launched server actually apply mod
//! mixins, each of which failed silently before (the game booted with the mod
//! list but no transformed class): the runtime namespace, the launcher's
//! class-bytes hand-off, the game classpath and the game's own entrypoint. Any
//! of them regressing turns every count below into zero, while the server still
//! reaches "Done".
//!
//! `HORIZON_MIXIN_CORPUS` names the jars to run (a path list, like `PATH`);
//! without it the test skips. Each jar runs alone in its own bounded launch and
//! must resolve its dependencies: pass self-sufficient production jars (for
//! example `lithium`, `ferritecore`, `c2me`), not mods that need Fabric API.

use std::{fs, path::PathBuf, process::Command};

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// `HORIZON_MIXIN_CORPUS` as the ordered list of jars to launch.
fn corpus_jars() -> Vec<PathBuf> {
    let Some(value) = std::env::var_os("HORIZON_MIXIN_CORPUS") else {
        return Vec::new();
    };
    std::env::split_paths(&value)
        .filter(|path| path.is_file())
        .collect()
}

/// `applied=<n> configs=<k> targets=<t> failed=<m>` from the audit line.
fn audit_counts(line: &str) -> Option<(usize, usize, usize, usize)> {
    let rest = line.strip_prefix("mixin-audit: ")?;
    let mut applied = None;
    let mut configs = None;
    let mut targets = None;
    let mut failed = None;
    for field in rest.split_whitespace() {
        let (name, value) = field.split_once('=')?;
        let value: usize = value.parse().ok()?;
        match name {
            "applied" => applied = Some(value),
            "configs" => configs = Some(value),
            "targets" => targets = Some(value),
            "failed" => failed = Some(value),
            _ => {}
        }
    }
    Some((applied?, configs?, targets?, failed?))
}

#[test]
fn real_corpus_mods_apply_their_mixins_in_a_launch() {
    let jars = corpus_jars();
    let (Some(game), Some(loader), Some(libs)) = (
        env_path("HORIZON_MINECRAFT_JAR"),
        env_path("FABRIC_LOADER_JAR"),
        env_path("FABRIC_LIBS_DIR"),
    ) else {
        eprintln!("skipping: set HORIZON_MINECRAFT_JAR, FABRIC_LOADER_JAR and FABRIC_LIBS_DIR");
        return;
    };
    if jars.is_empty() {
        eprintln!("skipping: HORIZON_MIXIN_CORPUS names no jar");
        return;
    }
    if std::env::var("HORIZON_REAL_LAUNCH").as_deref() != Ok("1") {
        eprintln!("skipping: set HORIZON_REAL_LAUNCH=1 to run the real launches");
        return;
    }

    let mut problems = Vec::new();
    for jar in &jars {
        let root = std::env::temp_dir().join(format!(
            "horizon-mixin-corpus-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_horizon-jvm"))
            .current_dir(&root)
            .env("HORIZON_REAL_LAUNCH", "1")
            .env("HORIZON_LAUNCH_STOP_AT_DONE", "1")
            .env("HORIZON_LAUNCH_TIMEOUT_SECS", "200")
            .arg("--real-loader")
            .arg("--entry-kind")
            .arg("server")
            .arg("--launch-game")
            .arg("--accept-eula")
            .arg("--mixin-audit")
            .arg("--loader")
            .arg(&loader)
            .arg("--libs")
            .arg(&libs)
            .arg("--game-jar")
            .arg(&game)
            .arg("--game-version")
            .arg("26.3")
            .arg(jar)
            .output()
            .expect("horizon-jvm should start");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let name = jar
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("mod.jar");
        let Some(line) = text.lines().find(|line| line.starts_with("mixin-audit: ")) else {
            problems.push(format!("{name}: no mixin-audit line:\n{text}"));
            let _ = fs::remove_dir_all(root);
            continue;
        };
        let Some((applied, _configs, _targets, failed)) = audit_counts(line) else {
            problems.push(format!("{name}: unreadable audit line '{line}'"));
            let _ = fs::remove_dir_all(root);
            continue;
        };
        let launched = text
            .lines()
            .any(|line| line.starts_with("launch-game: exit "));
        if applied == 0 || failed > 0 || !launched {
            problems.push(format!("{name}: {line} (server started: {launched})"));
        }
        println!("{name}: {line}");
        let _ = fs::remove_dir_all(root);
    }
    assert!(
        problems.is_empty(),
        "real corpus mods did not apply their mixins:\n{}",
        problems.join("\n")
    );
}

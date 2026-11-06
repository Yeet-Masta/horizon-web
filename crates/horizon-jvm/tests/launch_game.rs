//! `--launch-game`: the two-part gate and the bounded child that starts the
//! real game through Knot.
//!
//! The gate test never launches anything: it runs with
//! `HORIZON_REAL_LAUNCH` explicitly removed and asserts the skip line.
//!
//! The launch test needs the same local Fabric/Minecraft installation as
//! `classpath_unlock` (`HORIZON_MINECRAFT_JAR`, `FABRIC_LOADER_JAR`,
//! `FABRIC_LIBS_DIR`) *and* `HORIZON_REAL_LAUNCH=1` set for the test itself, so
//! a plain `cargo test` sweep never starts a game. The game runs in a fresh
//! temporary directory, so the server's `server.properties` and `eula.txt`
//! lookups never touch anyone's installation; a server with no EULA stops on
//! its own, and the wall clock is bounded either way.

use std::{fs, io::Write, path::PathBuf, process::Command, time::Instant};
use zip::write::SimpleFileOptions;

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// A jar Fabric discovers as a mod but that declares no entrypoint.
fn mod_jar(root: &std::path::Path) -> PathBuf {
    let jar = root.join("launchfixture.jar");
    let mut zip = zip::ZipWriter::new(fs::File::create(&jar).unwrap());
    zip.start_file("fabric.mod.json", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(br#"{"schemaVersion":1,"id":"launchfixture","version":"1.0"}"#)
        .unwrap();
    zip.finish().unwrap();
    jar
}

fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "horizon-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn the_flag_without_the_environment_gate_never_launches() {
    let root = temp_root("launch-gate");
    let jar = mod_jar(&root);
    let output = Command::new(env!("CARGO_BIN_EXE_horizon-jvm"))
        .current_dir(&root)
        .env_remove("HORIZON_REAL_LAUNCH")
        .arg("--real-loader")
        .arg("--entry-kind")
        .arg("server")
        .arg("--launch-game")
        .arg("--provider-report")
        .arg("--json")
        .arg(&jar)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "run failed:\n{text}");
    let line = text
        .lines()
        .find(|line| line.starts_with("launch-game: "))
        .unwrap_or_else(|| panic!("no launch-game line:\n{text}"));
    assert_eq!(line, "launch-game: skipped (HORIZON_REAL_LAUNCH is not 1)");
    assert!(!text.contains("launch-game: started"), "{text}");
    let report = text
        .lines()
        .find(|line| line.starts_with('{'))
        .unwrap_or_else(|| panic!("no JSON provider report:\n{text}"));
    let value: serde_json::Value = serde_json::from_str(report).unwrap();
    assert_eq!(
        value["launchGame"],
        "skipped (HORIZON_REAL_LAUNCH is not 1)"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_gated_launch_starts_the_real_game_in_a_bounded_child() {
    let (Some(game), Some(loader), Some(libs)) = (
        env_path("HORIZON_MINECRAFT_JAR"),
        env_path("FABRIC_LOADER_JAR"),
        env_path("FABRIC_LIBS_DIR"),
    ) else {
        eprintln!("skipping: set HORIZON_MINECRAFT_JAR, FABRIC_LOADER_JAR and FABRIC_LIBS_DIR");
        return;
    };
    if std::env::var("HORIZON_REAL_LAUNCH").as_deref() != Ok("1") {
        eprintln!("skipping: set HORIZON_REAL_LAUNCH=1 to run the real launch");
        return;
    }
    assert!(game.is_file(), "{} is not a file", game.display());
    assert!(loader.is_file(), "{} is not a file", loader.display());
    assert!(libs.is_dir(), "{} is not a directory", libs.display());

    let root = temp_root("launch-game");
    let jar = mod_jar(&root);
    let timeout = 60u64;
    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_horizon-jvm"))
        .current_dir(&root)
        .env("HORIZON_REAL_LAUNCH", "1")
        .env("HORIZON_LAUNCH_TIMEOUT_SECS", timeout.to_string())
        .arg("--real-loader")
        .arg("--entry-kind")
        .arg("server")
        .arg("--launch-game")
        .arg("--provider-report")
        .arg("--json")
        .arg("--loader")
        .arg(&loader)
        .arg("--libs")
        .arg(&libs)
        .arg("--game-jar")
        .arg(&game)
        .arg(&jar)
        .output()
        .unwrap();
    let elapsed = started.elapsed().as_secs();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "launch run failed:\n{text}");
    // The parent bounds the child, so the whole run cannot outlive the timeout
    // (plus startup slack for the parent's own work).
    assert!(
        elapsed <= timeout + 20,
        "the launch was not bounded: {elapsed}s\n{text}"
    );

    let line = text
        .lines()
        .find(|line| line.starts_with("launch-game: "))
        .unwrap_or_else(|| panic!("no launch-game line:\n{text}"));
    assert!(line.len() <= "launch-game: ".len() + 400, "{line}");
    let status = line.trim_start_matches("launch-game: ");
    assert!(
        status.starts_with("exit ") || status.starts_with("failed "),
        "unexpected launch status: {line}\n{text}"
    );
    // A local server with no eula.txt stops with `exit 0`; a missing game or a
    // failed launch is still an honest report.
    let report = text
        .lines()
        .find(|line| line.starts_with('{'))
        .unwrap_or_else(|| panic!("no JSON provider report:\n{text}"));
    let value: serde_json::Value = serde_json::from_str(report).unwrap();
    assert_eq!(value["launchGame"], status);
    let _ = fs::remove_dir_all(root);
}

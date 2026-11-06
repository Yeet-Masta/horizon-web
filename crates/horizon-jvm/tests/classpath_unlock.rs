//! Runs the production provider with `--classpath-unlock` when a Minecraft
//! 26.3 jar, a Fabric loader jar and its libraries are available
//! (`HORIZON_MINECRAFT_JAR`, `FABRIC_LOADER_JAR`, `FABRIC_LIBS_DIR`). Without
//! them the test skips: none of those files belong in the repo.
//!
//! The unlock is the loader's own registry: `GameProvider.unlockClassPath`
//! for the game jars and `FabricLoaderImpl.freeze` -> `finishModLoading` for
//! the mods, both landing on `KnotClassLoaderInterface.addCodeSource`. The
//! probe only reports `ok` after reading a real class back through that
//! loader, so this asserts a genuine classpath rather than a returned value.

use std::{fs, io::Write, path::PathBuf, process::Command};
use zip::write::SimpleFileOptions;

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// A jar Fabric discovers as a mod but that declares no entrypoint, so the
/// probe never invokes mod code: only the classpath machinery runs.
fn mod_jar(root: &std::path::Path) -> PathBuf {
    let jar = root.join("unlockfixture.jar");
    let mut zip = zip::ZipWriter::new(fs::File::create(&jar).unwrap());
    let options = SimpleFileOptions::default();
    zip.start_file("fabric.mod.json", options).unwrap();
    zip.write_all(br#"{"schemaVersion":1,"id":"unlockfixture","version":"1.0"}"#)
        .unwrap();
    zip.finish().unwrap();
    jar
}

fn run(root: &std::path::Path, jar: &PathBuf, unlock: bool) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_horizon-jvm"));
    command
        .current_dir(root)
        .arg("--real-loader")
        .arg("--entry-kind")
        .arg("server")
        .arg("--provider-report")
        .arg("--loader")
        .arg(env_path("FABRIC_LOADER_JAR").unwrap())
        .arg("--libs")
        .arg(env_path("FABRIC_LIBS_DIR").unwrap())
        .arg("--game-jar")
        .arg(env_path("HORIZON_MINECRAFT_JAR").unwrap());
    if unlock {
        command.arg("--classpath-unlock");
    }
    command.arg(jar);
    let output = command.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "provider run failed:\n{text}");
    text
}

#[test]
fn a_real_provider_unlocks_the_knot_classpath_when_asked() {
    let (Some(game), Some(loader), Some(libs)) = (
        env_path("HORIZON_MINECRAFT_JAR"),
        env_path("FABRIC_LOADER_JAR"),
        env_path("FABRIC_LIBS_DIR"),
    ) else {
        eprintln!("skipping: set HORIZON_MINECRAFT_JAR, FABRIC_LOADER_JAR and FABRIC_LIBS_DIR");
        return;
    };
    assert!(game.is_file(), "{} is not a file", game.display());
    assert!(loader.is_file(), "{} is not a file", loader.display());
    assert!(libs.is_dir(), "{} is not a directory", libs.display());

    let root = std::env::temp_dir().join(format!(
        "horizon-classpath-unlock-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    let jar = mod_jar(&root);

    // Default off: the flag's line never appears, and the run still succeeds.
    let plain = run(&root, &jar, false);
    assert!(
        plain.contains("real loader: entered production game provider"),
        "{plain}"
    );
    assert!(
        !plain.contains("classpath-unlock"),
        "the flag is opt-in:\n{plain}"
    );

    // With the flag: exactly one bounded line, and it says what happened.
    let unlocked = run(&root, &jar, true);
    assert!(
        unlocked.contains("real loader: entered production game provider"),
        "{unlocked}"
    );
    assert!(
        !unlocked.contains("classpath-unlock: failed"),
        "the unlock failed:\n{unlocked}"
    );
    let line = unlocked
        .lines()
        .find(|line| line.starts_with("classpath-unlock: "))
        .unwrap_or_else(|| panic!("no classpath-unlock line:\n{unlocked}"));
    assert!(
        line.starts_with("classpath-unlock: ok "),
        "unexpected unlock line: {line}"
    );
    assert!(
        line.len() <= "classpath-unlock: ".len() + 400,
        "unlock line is not bounded: {line}"
    );
    assert!(
        line.ends_with(" jars"),
        "the line states how many jars were unlocked: {line}"
    );

    let _ = fs::remove_dir_all(root);
}

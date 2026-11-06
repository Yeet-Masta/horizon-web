//! Gated production-provider check for Knot transformer initialization.
//! Requires the same local Fabric/Minecraft installation as classpath_unlock.

use std::{fs, io::Write, path::PathBuf, process::Command};
use zip::write::SimpleFileOptions;

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

#[test]
fn a_real_provider_reports_knot_transformer_initialization() {
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
        "horizon-knot-init-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    let jar = root.join("knotinitfixture.jar");
    let mut zip = zip::ZipWriter::new(fs::File::create(&jar).unwrap());
    zip.start_file("fabric.mod.json", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(br#"{"schemaVersion":1,"id":"knotinitfixture","version":"1.0"}"#)
        .unwrap();
    zip.finish().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_horizon-jvm"))
        .current_dir(&root)
        .arg("--real-loader")
        .arg("--entry-kind")
        .arg("server")
        .arg("--classpath-unlock")
        .arg("--provider-report")
        .arg("--loader")
        .arg(&loader)
        .arg("--libs")
        .arg(&libs)
        .arg("--game-jar")
        .arg(&game)
        .arg(&jar)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "provider run failed:\n{text}");
    assert!(
        text.contains("real loader: entered production game provider"),
        "{text}"
    );
    let line = text
        .lines()
        .find(|line| line.starts_with("knot-init: "))
        .unwrap_or_else(|| panic!("no knot-init line:\n{text}"));
    assert!(
        line.starts_with("knot-init: ok "),
        "unexpected line: {line}"
    );
    assert!(line.len() <= "knot-init: ".len() + 400);
    assert!(text.contains("knot-init: ok 1 transformers\n"), "{text}");

    let _ = fs::remove_dir_all(root);
}

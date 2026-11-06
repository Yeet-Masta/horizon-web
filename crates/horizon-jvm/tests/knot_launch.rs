//! Gated production-provider check for `--knot-launch`: the Knot class loader
//! becomes the launcher's target, and a class loaded through it comes back
//! transformed.
//!
//! The fixture mod carries one class whose `message()` returns the marker
//! `KNOT-RAW`. The probe installs a marker-rewriting transformer around the
//! delegate's `IMixinTransformer`, loads that class through Knot and reads it
//! back: `KNOT-OK!` proves the bytes the JVM defined travelled
//! `getPostMixinClassByteArray` -> `IMixinTransformer.transformClassBytes`,
//! while the raw jar on disk still holds `KNOT-RAW`.
//!
//! Needs the same local Fabric/Minecraft installation as classpath_unlock
//! (`HORIZON_MINECRAFT_JAR`, `FABRIC_LOADER_JAR`, `FABRIC_LIBS_DIR`) plus a
//! `javac`; none of those files belong in the repo, so it skips without them.

use std::{fs, io::Write, path::PathBuf, process::Command};
use zip::write::SimpleFileOptions;

/// The marker constant the probe transformer rewrites (same length, so the
/// class file keeps its constant-pool layout).
const RAW: &str = "KNOT-RAW";
const PROBE_CLASS: &str = "knotlaunchprobe.Marker";

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// Compile the marker class with the available JDK; `None` when javac is absent.
fn compile_marker(root: &std::path::Path) -> Option<PathBuf> {
    let package = root.join("src").join("knotlaunchprobe");
    fs::create_dir_all(&package).unwrap();
    let source = package.join("Marker.java");
    fs::write(
        &source,
        format!(
            "package knotlaunchprobe;\n\npublic final class Marker {{\n  public static String message() {{ return \"{RAW}\"; }}\n}}\n"
        ),
    )
    .unwrap();
    let classes = root.join("classes");
    fs::create_dir_all(&classes).unwrap();
    let javac = std::env::var_os("JAVA_HOME")
        .map(PathBuf::from)
        .map(|home| home.join("bin").join("javac"))
        .unwrap_or_else(|| PathBuf::from("javac"));
    let Ok(status) = Command::new(javac)
        .arg("-Xlint:none")
        .arg("-nowarn")
        .arg("-d")
        .arg(&classes)
        .arg(&source)
        .status()
    else {
        return None;
    };
    status
        .success()
        .then(|| classes.join("knotlaunchprobe").join("Marker.class"))
}

/// A mod jar carrying the marker class, so Fabric registers its code source.
fn fixture_jar(root: &std::path::Path, marker_class: &std::path::Path) -> PathBuf {
    let jar = root.join("knotlaunchfixture.jar");
    let mut zip = zip::ZipWriter::new(fs::File::create(&jar).unwrap());
    let options = SimpleFileOptions::default();
    zip.start_file("fabric.mod.json", options).unwrap();
    zip.write_all(br#"{"schemaVersion":1,"id":"knotlaunchfixture","version":"1.0"}"#)
        .unwrap();
    zip.start_file("knotlaunchprobe/Marker.class", options)
        .unwrap();
    zip.write_all(&fs::read(marker_class).unwrap()).unwrap();
    zip.finish().unwrap();
    jar
}

#[test]
fn a_real_provider_hands_knot_to_the_launcher_and_transforms_a_class() {
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
        "horizon-knot-launch-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    let Some(marker_class) = compile_marker(&root) else {
        eprintln!("skipping: no javac available");
        let _ = fs::remove_dir_all(root);
        return;
    };
    assert!(
        marker_class.is_file(),
        "{} was not compiled",
        marker_class.display()
    );
    // The fixture jar itself still carries the raw marker; only the class the
    // loader defines may differ.
    assert!(
        fs::read(&marker_class)
            .unwrap()
            .windows(RAW.len())
            .any(|window| window == RAW.as_bytes())
    );
    let jar = fixture_jar(&root, &marker_class);

    let output = Command::new(env!("CARGO_BIN_EXE_horizon-jvm"))
        .current_dir(&root)
        .arg("--real-loader")
        .arg("--entry-kind")
        .arg("server")
        .arg("--knot-launch")
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

    // One bounded stdout line naming the class the chain rewrote.
    let expected = format!("ok 1 transformers, transformed={PROBE_CLASS}");
    let line = text
        .lines()
        .find(|line| line.starts_with("knot-launch: "))
        .unwrap_or_else(|| panic!("no knot-launch line:\n{text}"));
    assert_eq!(line, format!("knot-launch: {expected}"));
    assert!(line.len() <= "knot-launch: ".len() + 400);

    // The JSON provider report carries the same one line.
    let report = text
        .lines()
        .find(|line| line.starts_with('{'))
        .unwrap_or_else(|| panic!("no JSON provider report:\n{text}"));
    let value: serde_json::Value = serde_json::from_str(report).unwrap();
    assert_eq!(value["knotLaunch"], expected.as_str());

    // The probe transformed nothing else and never launched the game.
    assert!(!text.contains("real-launch: stub-entrypoint-ok"), "{text}");

    let _ = fs::remove_dir_all(root);
}

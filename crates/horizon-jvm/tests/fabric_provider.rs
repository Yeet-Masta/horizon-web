//! Runs a real Fabric server mod through the production `MinecraftGameProvider`
//! when a Minecraft 26.3 server jar, a Fabric loader jar and its libraries are
//! available (`HORIZON_MINECRAFT_JAR`, `FABRIC_LOADER_JAR`, `FABRIC_LIBS_DIR`).
//! Without them the test skips: none of those files belong in the repo.

use std::{fs, io::Write, path::PathBuf, process::Command};
use zip::write::SimpleFileOptions;

const MOD_SOURCE: &str = r#"
package demo;
public final class Real implements net.fabricmc.api.ModInitializer,
    net.fabricmc.api.DedicatedServerModInitializer {
  @Override public void onInitialize() { System.out.println("real-game-provider-ok"); }
  @Override public void onInitializeServer() { System.out.println("real-game-server-entrypoint-ok"); }
}
"#;

fn javac() -> PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into())
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

#[test]
fn a_real_game_jar_enters_the_production_provider() {
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
        "horizon-fabric-provider-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let source = root.join("demo/Real.java");
    let classes = root.join("classes");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::create_dir_all(&classes).unwrap();
    fs::write(&source, MOD_SOURCE).unwrap();
    let status = Command::new(javac())
        .arg("--release")
        .arg("17")
        .arg("-cp")
        .arg(&loader)
        .arg("-d")
        .arg(&classes)
        .arg(&source)
        .status()
        .unwrap();
    assert!(status.success(), "javac failed");

    let jar = root.join("realfixture.jar");
    let mut zip = zip::ZipWriter::new(fs::File::create(&jar).unwrap());
    let options = SimpleFileOptions::default();
    zip.start_file("fabric.mod.json", options).unwrap();
    zip.write_all(
        br#"{"schemaVersion":1,"id":"realfixture","version":"1.0","entrypoints":{"server":["demo.Real"]}}"#,
    )
    .unwrap();
    zip.start_file("demo/Real.class", options).unwrap();
    zip.write_all(&fs::read(classes.join("demo/Real.class")).unwrap())
        .unwrap();
    zip.finish().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_horizon-jvm"))
        .current_dir(&root)
        .arg("--real-loader")
        .arg("--entry-kind")
        .arg("server")
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
    assert!(text.contains("minecraft 26.3"), "{text}");
    assert!(text.contains("real-game-server-entrypoint-ok"), "{text}");
    let _ = fs::remove_dir_all(root);
}

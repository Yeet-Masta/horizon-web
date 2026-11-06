//! Runs a real NeoForge `@Mod` through `horizon-jvm --real-loader` when an
//! installed NeoForge server is available (`NEOFORGE_SERVER_DIR`). Without
//! one the test skips, because a real FML install is not part of the repo.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};
use zip::write::SimpleFileOptions;

const MOD_SOURCE: &str = r#"
package demo;
import net.neoforged.bus.api.IEventBus;
@net.neoforged.fml.common.Mod("fmlfixture")
public final class RealMod {
  public RealMod(IEventBus bus) { System.out.println("fml-fixture-ok:" + getClass().getName()); }
}
"#;

const MOD_TOML: &str = r#"modLoader = "javafml"
loaderVersion = "[1,)"
license = "MIT"

[[mods]]
modId = "fmlfixture"
version = "1.0"
displayName = "FML Fixture"
authors = "Horizon"
description = "A real NeoForge fixture."
"#;

fn javac() -> PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into())
}

fn classpath(libs: &Path) -> Option<String> {
    let mut args: Vec<PathBuf> = fs::read_dir(libs.join("net/neoforged/neoforge"))
        .ok()?
        .flatten()
        .map(|entry| entry.path().join("unix_args.txt"))
        .filter(|path| path.is_file())
        .collect();
    args.sort();
    let text = fs::read_to_string(args.pop()?).ok()?;
    let mut lines = text.lines();
    let index = lines.position(|line| line == "-classpath")?;
    let entries = text.lines().nth(index + 1)?;
    let server = libs.parent().unwrap_or(libs);
    Some(
        entries
            .split(':')
            .map(|entry| {
                if Path::new(entry).is_absolute() {
                    PathBuf::from(entry)
                } else {
                    server.join(entry)
                }
            })
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":"),
    )
}

#[test]
fn fml_loader_runs_a_real_mod_when_installed() {
    let Some(server) = std::env::var_os("NEOFORGE_SERVER_DIR") else {
        eprintln!("skipping: NEOFORGE_SERVER_DIR is not set");
        return;
    };
    let server = PathBuf::from(server);
    let libs = server.join("libraries");
    let Some(classpath) = classpath(&libs) else {
        eprintln!(
            "skipping: {} is not an installed NeoForge server",
            server.display()
        );
        return;
    };
    let root = std::env::temp_dir().join(format!(
        "horizon-fml-it-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let src = root.join("demo/RealMod.java");
    let classes = root.join("classes");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::create_dir_all(&classes).unwrap();
    fs::write(&src, MOD_SOURCE).unwrap();
    let status = Command::new(javac())
        .arg("--release")
        .arg("17")
        .arg("-cp")
        .arg(&classpath)
        .arg("-d")
        .arg(&classes)
        .arg(&src)
        .status()
        .unwrap();
    assert!(status.success(), "javac failed");

    let jar = root.join("fml-fixture.jar");
    let mut zip = zip::ZipWriter::new(fs::File::create(&jar).unwrap());
    zip.start_file("META-INF/neoforge.mods.toml", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(MOD_TOML.as_bytes()).unwrap();
    zip.start_file("demo/RealMod.class", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(&fs::read(classes.join("demo/RealMod.class")).unwrap())
        .unwrap();
    zip.finish().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_horizon-jvm"))
        .arg("--real-loader")
        .arg("--entry-kind")
        .arg("neoforge")
        .arg("--libs")
        .arg(&libs)
        .arg(&jar)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "FML probe failed:\n{text}");
    assert!(text.contains("fml-fixture-ok:demo.RealMod"), "{text}");
    assert!(text.contains("real loader: FML probe completed"), "{text}");
    let _ = fs::remove_dir_all(root);
}

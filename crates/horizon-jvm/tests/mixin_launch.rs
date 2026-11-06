//! A mod's own `@Mixin` applies during `--launch-game`, through the real
//! Fabric loader and Knot, and the injected callback changes what the mod's
//! code returns at runtime.
//!
//! The fixture is a three-class mod: `launchprobe.Target.value()` returns
//! `MIXIN-RAW`, and `launchprobe.mixin.TargetMixin` injects at the method head
//! and returns `MIXIN-OK!`. `@Mixin` is CLASS-retained (Mixin's own
//! declaration), so the JVM cannot see it: the entrypoint reports both the
//! value and `TargetMixin.class.isAnnotationPresent(Mixin.class)`, which is
//! `false` exactly because the annotation is invisible to reflection. Only a
//! transformer reading class bytes can make the call return the injected
//! value. The mod's `main` entrypoint logs the marker through the game's
//! logger, and the test reads it out of the launched server's
//! `logs/latest.log` — so a matching line proves the loader registered the
//! config, Knot transformed the target, and the callback ran in the launched
//! game (not only in a probe).
//!
//! Needs the same local Fabric/Minecraft installation as `classpath_unlock`
//! (`HORIZON_MINECRAFT_JAR`, `FABRIC_LOADER_JAR`, `FABRIC_LIBS_DIR`) and
//! `HORIZON_REAL_LAUNCH=1`, a `javac`, and `sponge-mixin` + `slf4j-api` jars
//! under `FABRIC_LIBS_DIR`; none of those files belong in the repo, so the test
//! skips without them.

use std::{fs, io::Write, path::PathBuf, process::Command};
use zip::write::SimpleFileOptions;

const TARGET: &str = r#"package launchprobe;

public final class Target {
  private Target() {}
  public static String value() {
    return "MIXIN-RAW";
  }
}
"#;

const MIXIN: &str = r#"package launchprobe.mixin;

import launchprobe.Target;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;

@Mixin(Target.class)
public final class TargetMixin {
  @Inject(method = "value", at = @At("HEAD"), cancellable = true)
  private static void horizon$value(CallbackInfoReturnable<String> cir) {
    cir.setReturnValue("MIXIN-OK!");
  }
}
"#;

const ENTRY: &str = r#"package launchprobe;

import java.lang.annotation.Retention;
import net.fabricmc.api.ModInitializer;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;
import org.spongepowered.asm.mixin.Mixin;

public final class ProbeEntry implements ModInitializer {
  private static final Logger LOGGER = LoggerFactory.getLogger("horizon-mixin-probe");

  @Override
  public void onInitialize() {
    // The game's log is the only place both the launched server and this test
    // can see; stdout belongs to the supervisor. `@Mixin` is CLASS-retained, so
    // the JVM can never read it off a class -- the value below is injected by
    // the transformer alone. (Loading the mixin class itself is forbidden by
    // Mixin, hence the annotation's own `@Retention` rather than a reflection
    // read on `TargetMixin`.)
    Retention retention = Mixin.class.getAnnotation(Retention.class);
    LOGGER.info("HORIZON-MIXIN-PROBE value={} mixinRetention={}", Target.value(),
        retention == null ? "none" : retention.value());
  }
}
"#;

const MIXIN_CONFIG: &str = r#"{
  "required": true,
  "package": "launchprobe.mixin",
  "compatibilityLevel": "JAVA_21",
  "mixins": ["TargetMixin"]
}
"#;

const MOD_JSON: &str = r#"{"schemaVersion":1,"id":"mixlaunchprobe","version":"1.0","entrypoints":{"main":["launchprobe.ProbeEntry"]},"mixins":["mixlaunchprobe.mixins.json"]}"#;

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// The first jar under the loader's libraries whose file name matches.
fn find_lib(libs: &std::path::Path, prefix: &str) -> Option<PathBuf> {
    let mut stack = vec![libs.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(prefix) && name.ends_with(".jar"))
            {
                return Some(path);
            }
        }
    }
    None
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

/// Compile target + mixin + entrypoint, then pack the mod jar.
fn fixture_jar(
    root: &std::path::Path,
    loader_jar: &std::path::Path,
    mixin_jar: &std::path::Path,
    slf4j_jar: &std::path::Path,
) -> Option<PathBuf> {
    let sources = root.join("src");
    let mut files = Vec::new();
    for (relative, text) in [
        ("launchprobe/Target.java", TARGET),
        ("launchprobe/mixin/TargetMixin.java", MIXIN),
        ("launchprobe/ProbeEntry.java", ENTRY),
    ] {
        let path = sources.join(relative);
        fs::create_dir_all(path.parent()?).ok()?;
        fs::write(&path, text).ok()?;
        files.push(path);
    }
    let classes = root.join("classes");
    fs::create_dir_all(&classes).ok()?;
    let javac = std::env::var_os("JAVA_HOME")
        .map(PathBuf::from)
        .map(|home| home.join("bin").join("javac"))
        .unwrap_or_else(|| PathBuf::from("javac"));
    let classpath = std::env::join_paths([loader_jar, mixin_jar, slf4j_jar]).ok()?;
    let mut command = Command::new(javac);
    command
        .arg("--release")
        .arg("21")
        .arg("-Xlint:none")
        .arg("-nowarn")
        .arg("-cp")
        .arg(classpath)
        .arg("-d")
        .arg(&classes);
    command.args(&files);
    let status = command.status().ok()?;
    if !status.success() {
        return None;
    }

    let jar = root.join("mixlaunchprobe.jar");
    let mut zip = zip::ZipWriter::new(fs::File::create(&jar).ok()?);
    let options = SimpleFileOptions::default();
    for (name, bytes) in [
        ("fabric.mod.json", MOD_JSON.as_bytes().to_vec()),
        (
            "mixlaunchprobe.mixins.json",
            MIXIN_CONFIG.as_bytes().to_vec(),
        ),
    ] {
        zip.start_file(name, options).ok()?;
        zip.write_all(&bytes).ok()?;
    }
    for entry in [
        "launchprobe/Target.class",
        "launchprobe/mixin/TargetMixin.class",
        "launchprobe/ProbeEntry.class",
    ] {
        zip.start_file(entry, options).ok()?;
        zip.write_all(&fs::read(classes.join(entry)).ok()?).ok()?;
    }
    zip.finish().ok()?;
    Some(jar)
}

/// The launch directory the supervisor created for this child.
fn launch_dir(pid: u32) -> Option<PathBuf> {
    let prefix = format!("horizon-launch-{pid}-");
    let mut found: Vec<PathBuf> = fs::read_dir(std::env::temp_dir())
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix))
        })
        .collect();
    found.sort();
    found.pop()
}

#[test]
fn a_mod_mixin_applies_inside_the_launched_server() {
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
    let (Some(mixin_jar), Some(slf4j_jar)) = (
        find_lib(&libs, "sponge-mixin"),
        find_lib(&libs, "slf4j-api"),
    ) else {
        eprintln!(
            "skipping: sponge-mixin and slf4j-api jars are required under {}",
            libs.display()
        );
        return;
    };

    let root = temp_root("mixin-launch");
    let Some(jar) = fixture_jar(&root, &loader, &mixin_jar, &slf4j_jar) else {
        eprintln!("skipping: could not compile the fixture mod (no javac?)");
        let _ = fs::remove_dir_all(root);
        return;
    };

    let child = Command::new(env!("CARGO_BIN_EXE_horizon-jvm"))
        .current_dir(&root)
        .env("HORIZON_REAL_LAUNCH", "1")
        .env("HORIZON_LAUNCH_STOP_AT_DONE", "1")
        .env("HORIZON_LAUNCH_TIMEOUT_SECS", "150")
        .arg("--real-loader")
        .arg("--entry-kind")
        .arg("server")
        .arg("--launch-game")
        .arg("--accept-eula")
        .arg("--mixin-audit")
        .arg("--provider-report")
        .arg("--json")
        .arg("--loader")
        .arg(&loader)
        .arg("--libs")
        .arg(&libs)
        .arg("--game-jar")
        .arg(&game)
        .arg("--game-version")
        .arg("26.3")
        .arg(&jar)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("horizon-jvm should start");
    let pid = child.id();
    let output = child.wait_with_output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "launch run failed:\n{text}");

    let dir =
        launch_dir(pid).unwrap_or_else(|| panic!("no launch directory for pid {pid}:\n{text}"));
    let log = fs::read_to_string(dir.join("game").join("logs").join("latest.log"))
        .unwrap_or_else(|error| panic!("no game log under {}: {error}\n{text}", dir.display()));
    assert!(
        log.contains("HORIZON-MIXIN-PROBE value=MIXIN-OK! mixinRetention=CLASS"),
        "the mod's mixin callback did not run in the launched game:\n{log}\n{text}"
    );
    assert!(
        !log.contains("value=MIXIN-RAW"),
        "the raw value reached the entrypoint, so the mixin did not apply:\n{log}"
    );
    // Mixin's own verbose record of the application, written by the transformer
    // Knot installed: the target is the mod's class, and the config is the
    // mod's.
    assert!(
        log.contains("Mixing TargetMixin from mixlaunchprobe.mixins.json into launchprobe.Target"),
        "no verbose mixin application line for the fixture:\n{log}"
    );

    let audit = text
        .lines()
        .find(|line| line.starts_with("mixin-audit: "))
        .unwrap_or_else(|| panic!("no mixin-audit line:\n{text}"));
    assert!(
        !audit.contains("applied=0 "),
        "the audit saw no applied mixin: {audit}\n{text}"
    );
    let report = text
        .lines()
        .find(|line| line.starts_with('{'))
        .unwrap_or_else(|| panic!("no JSON provider report:\n{text}"));
    let value: serde_json::Value = serde_json::from_str(report).unwrap();
    let launch = text
        .lines()
        .find_map(|line| line.strip_prefix("launch-game: "))
        .unwrap_or_else(|| panic!("no launch-game line:\n{text}"));
    assert_eq!(value["launchGame"], launch);

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(dir);
}

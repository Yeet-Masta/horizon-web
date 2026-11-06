//! Gated production-provider check for `--mixin-apply-probe`: a real mixin
//! config is registered by the Fabric loader and applied by Knot when a mod's
//! class is loaded, without starting the game.
//!
//! The fixture mod carries one target class (`value()` returns `MIXIN-RAW`) and
//! one mixin that injects at the method head and returns `MIXIN-OK!`. The probe
//! loads the target through `KnotClassLoaderInterface.loadIntoTarget` after
//! `initializeTransformers`, so the value proves `getPostMixinClassByteArray`
//! -> `IMixinTransformer` -> the registered config, not a probe transformer.
//!
//! Needs the same local Fabric/Minecraft installation as `classpath_unlock`
//! (`HORIZON_MINECRAFT_JAR`, `FABRIC_LOADER_JAR`, `FABRIC_LIBS_DIR`), a `javac`
//! and a `sponge-mixin` jar under `FABRIC_LIBS_DIR`; none of those files belong
//! in the repo, so it skips without them.

use std::{fs, io::Write, path::PathBuf, process::Command};
use zip::write::SimpleFileOptions;

const TARGET: &str = r#"package applyprobe;

public final class Target {
  private Target() {}
  public static String value() {
    return "MIXIN-RAW";
  }
}
"#;

const MIXIN: &str = r#"package applyprobe.mixin;

import applyprobe.Target;
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

const MIXIN_CONFIG: &str = r#"{
  "required": true,
  "package": "applyprobe.mixin",
  "compatibilityLevel": "JAVA_21",
  "mixins": ["TargetMixin"]
}
"#;

const MOD_JSON: &str = r#"{"schemaVersion":1,"id":"mixapplyprobe","version":"1.0","mixins":["mixapplyprobe.mixins.json"]}"#;

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// The first `sponge-mixin` jar under the loader's libraries.
fn find_mixin_jar(libs: &std::path::Path) -> Option<PathBuf> {
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
                .is_some_and(|name| name.starts_with("sponge-mixin") && name.ends_with(".jar"))
            {
                return Some(path);
            }
        }
    }
    None
}

/// Compile the two fixture classes against the mixin annotations jar.
fn compile_fixture(root: &std::path::Path, mixin_jar: &std::path::Path) -> Option<PathBuf> {
    let sources = root.join("src");
    let target = sources.join("applyprobe").join("Target.java");
    let mixin = sources
        .join("applyprobe")
        .join("mixin")
        .join("TargetMixin.java");
    fs::create_dir_all(target.parent()?).ok()?;
    fs::create_dir_all(mixin.parent()?).ok()?;
    fs::write(&target, TARGET).ok()?;
    fs::write(&mixin, MIXIN).ok()?;
    let classes = root.join("classes");
    fs::create_dir_all(&classes).ok()?;
    let javac = std::env::var_os("JAVA_HOME")
        .map(PathBuf::from)
        .map(|home| home.join("bin").join("javac"))
        .unwrap_or_else(|| PathBuf::from("javac"));
    let status = Command::new(javac)
        .arg("--release")
        .arg("21")
        .arg("-Xlint:none")
        .arg("-nowarn")
        .arg("-cp")
        .arg(mixin_jar)
        .arg("-d")
        .arg(&classes)
        .arg(&target)
        .arg(&mixin)
        .status()
        .ok()?;
    status.success().then_some(classes)
}

fn fixture_jar(root: &std::path::Path, classes: &std::path::Path) -> PathBuf {
    let jar = root.join("mixapplyprobe.jar");
    let mut zip = zip::ZipWriter::new(fs::File::create(&jar).unwrap());
    let options = SimpleFileOptions::default();
    zip.start_file("fabric.mod.json", options).unwrap();
    zip.write_all(MOD_JSON.as_bytes()).unwrap();
    zip.start_file("mixapplyprobe.mixins.json", options)
        .unwrap();
    zip.write_all(MIXIN_CONFIG.as_bytes()).unwrap();
    for (entry, file) in [
        (
            "applyprobe/Target.class",
            classes.join("applyprobe/Target.class"),
        ),
        (
            "applyprobe/mixin/TargetMixin.class",
            classes.join("applyprobe/mixin/TargetMixin.class"),
        ),
    ] {
        zip.start_file(entry, options).unwrap();
        zip.write_all(&fs::read(file).unwrap()).unwrap();
    }
    zip.finish().unwrap();
    jar
}

#[test]
fn a_real_provider_applies_a_mod_mixin_through_knot() {
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
    let Some(mixin_jar) = find_mixin_jar(&libs) else {
        eprintln!("skipping: no sponge-mixin jar under {}", libs.display());
        return;
    };

    let root = std::env::temp_dir().join(format!(
        "horizon-mixin-apply-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    let Some(classes) = compile_fixture(&root, &mixin_jar) else {
        eprintln!("skipping: could not compile the mixin fixture (no javac?)");
        let _ = fs::remove_dir_all(root);
        return;
    };
    let jar = fixture_jar(&root, &classes);

    let output = Command::new(env!("CARGO_BIN_EXE_horizon-jvm"))
        .current_dir(&root)
        .arg("--real-loader")
        .arg("--entry-kind")
        .arg("server")
        .arg("--knot-launch")
        .arg("--mixin-apply-probe")
        .arg("applyprobe.Target#value=MIXIN-OK!")
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

    let line = text
        .lines()
        .find(|line| line.starts_with("mixin-apply: "))
        .unwrap_or_else(|| panic!("no mixin-apply line:\n{text}"));
    assert_eq!(line, "mixin-apply: ok applied=applyprobe.Target#value");
    assert!(line.len() <= "mixin-apply: ".len() + 400, "{line}");
    let report = text
        .lines()
        .find(|line| line.starts_with('{'))
        .unwrap_or_else(|| panic!("no JSON provider report:\n{text}"));
    let value: serde_json::Value = serde_json::from_str(report).unwrap();
    assert_eq!(value["mixinApply"], "ok applied=applyprobe.Target#value");
    assert!(!text.contains("launch-game: started"), "{text}");

    let _ = fs::remove_dir_all(root);
}

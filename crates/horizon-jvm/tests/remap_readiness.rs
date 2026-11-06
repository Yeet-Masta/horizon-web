//! Gated checks for the local intermediary mappings and client install probe.

use std::{fs, path::PathBuf, process::Command};

fn mapping_path() -> Option<PathBuf> {
    let value = std::env::var_os("HORIZON_MAPPINGS")?;
    std::env::split_paths(&value).find(|path| path.is_file())
}

#[test]
fn real_mappings_report_a_built_tree_when_installed() {
    let Some(path) = mapping_path() else {
        eprintln!("skipping: HORIZON_MAPPINGS does not name a real mappings file");
        return;
    };
    let output = Command::new(env!("CARGO_BIN_EXE_horizon-jvm"))
        .arg("--remap-report")
        .arg("--mappings")
        .arg(&path)
        .output()
        .expect("probe should start");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "probe failed: {text}");
    assert!(
        text.lines().any(|line| line.starts_with("remap: ok ")),
        "{text}"
    );
}

#[test]
fn client_readiness_is_bounded_without_starting_a_game() {
    let root = tempfile_root();
    let output = Command::new(env!("CARGO_BIN_EXE_horizon-jvm"))
        .arg("--entry-kind")
        .arg("client")
        .arg("--client-home")
        .arg(&root)
        .arg("--provider-report")
        .arg("--json")
        .output()
        .expect("probe should start");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "probe failed: {text}");
    assert!(text.contains("client-launch: needs assets ("), "{text}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn installed_client_readiness_resolves_game_jar_assets_and_natives() {
    let home = std::env::var_os("HORIZON_MINECRAFT_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|value| PathBuf::from(value).join(".minecraft")));
    let Some(home) = home else {
        eprintln!("skipping: no Minecraft home");
        return;
    };
    let version = "26.3";
    let jar = home
        .join("versions")
        .join(version)
        .join(format!("{version}.jar"));
    let asset_id = fs::read_to_string(
        home.join("versions")
            .join(version)
            .join(format!("{version}.json")),
    )
    .ok()
    .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    .and_then(|value| {
        value
            .get("assetIndex")?
            .get("id")?
            .as_str()
            .map(str::to_owned)
    })
    .unwrap_or_else(|| version.to_owned());
    let assets = home
        .join("assets")
        .join("indexes")
        .join(format!("{asset_id}.json"));
    let natives = home.join("versions").join(version).join("natives");
    let library_natives = has_library_native(&home.join("libraries"));
    if !jar.is_file() || !assets.is_file() || (!natives.is_dir() && !library_natives) {
        eprintln!(
            "skipping: client jar/assets/natives are not installed under {}",
            home.display()
        );
        return;
    }
    let output = Command::new(env!("CARGO_BIN_EXE_horizon-jvm"))
        .arg("--entry-kind")
        .arg("client")
        .arg("--game-version")
        .arg(version)
        .arg("--game-jar")
        .arg(&jar)
        .arg("--provider-report")
        .arg("--json")
        .output()
        .expect("probe should start");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "probe failed: {text}");
    assert!(text.contains("client-launch: ready"), "{text}");
}

fn has_library_native(root: &std::path::Path) -> bool {
    let Ok(entries) = fs::read_dir(root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            has_library_native(&path)
        } else {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("natives-linux.jar"))
        }
    })
}

fn tempfile_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "horizon-jvm-readiness-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("temporary readiness root");
    root
}

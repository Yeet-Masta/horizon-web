//! Opt-in checks for the production Fabric bootstrap extensions.

use std::{env, path::PathBuf, process::Command};

fn path(name: &str) -> Option<PathBuf> {
    env::var_os(name).map(PathBuf::from)
}

#[test]
fn production_bootstrap_reports_development_freeze_launch_and_discovery() {
    let (Some(game), Some(loader), Some(libs)) = (
        path("HORIZON_MINECRAFT_JAR"),
        path("FABRIC_LOADER_JAR"),
        path("FABRIC_LIBS_DIR"),
    ) else {
        eprintln!("skipping: set HORIZON_MINECRAFT_JAR, FABRIC_LOADER_JAR and FABRIC_LIBS_DIR");
        return;
    };
    let mod_jar = path("HORIZON_FABRIC_MOD_JAR")
        .or_else(|| Some(PathBuf::from("/tmp/opencode/fabmod/realfixture.jar")));
    let Some(mod_jar) = mod_jar.filter(|jar| jar.is_file()) else {
        eprintln!(
            "skipping: set HORIZON_FABRIC_MOD_JAR or provide /tmp/opencode/fabmod/realfixture.jar"
        );
        return;
    };
    let output = Command::new(env!("CARGO_BIN_EXE_horizon-jvm"))
        .arg("--real-loader")
        .arg("--real-launch")
        .arg("--fabric-discovery")
        .arg("--entry-kind")
        .arg("server")
        .arg("--loader")
        .arg(loader)
        .arg("--libs")
        .arg(libs)
        .arg("--game-jar")
        .arg(game)
        .arg(mod_jar)
        .output()
        .expect("run horizon-jvm");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "production bootstrap failed:\n{text}"
    );
    assert!(
        text.contains("real loader: entered production game provider"),
        "{text}"
    );
    assert!(text.contains("real loader: isDevelopment=false"), "{text}");
    assert!(text.contains("real-launch: stub-entrypoint-ok"), "{text}");
    assert!(text.contains("discovery:"), "{text}");
}

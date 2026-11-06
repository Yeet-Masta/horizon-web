//! Runtime-built fixture for bounded Mixin JSON config discovery.
//!
//! The jar holds only metadata and JSON, so no `javac` is involved: the test
//! covers the loader-facing config rows, not class preparation.

use horizon_native_java::{MixinConfig, NativeJar};
use std::{fs, io::Write, path::PathBuf};
use zip::write::SimpleFileOptions;

fn build_jar(name: &str, files: &[(&str, &[u8])]) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-mixin-config-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let jar_path = root.join(format!("{name}.jar"));
    let mut zip = zip::ZipWriter::new(fs::File::create(&jar_path).unwrap());
    for (entry, bytes) in files {
        zip.start_file(*entry, SimpleFileOptions::default())
            .unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
    (root, jar_path)
}

#[test]
fn reads_fabric_declared_and_root_configs() {
    let (root, jar_path) = build_jar(
        "configs",
        &[
            (
                "fabric.mod.json",
                br#"{"id":"mixin-config-fixture","version":"1.0","mixins":["demo.mixins.json",{"config":"extra.mixins.json","environment":"client"}]}"#,
            ),
            (
                "demo.mixins.json",
                br#"{"required":false,"package":"demo.mixin","plugin":"demo.Plugin","refmap":"demo.refmap.json","priority":1500,"mixinPriority":1200,"mixins":["AlphaMixin","beta.GammaMixin"],"client":["client.ClientMixin"],"server":["server.ServerMixin"]}"#,
            ),
            (
                "extra.mixins.json",
                br#"{"package":"demo.extra","mixins":["ExtraMixin"]}"#,
            ),
            (
                "nested/nested.mixins.json",
                br#"{"package":"nested","mixins":["NestedMixin"]}"#,
            ),
        ],
    );

    let jar = NativeJar::from_path(&jar_path).unwrap();
    let configs = jar.mixin_configs();

    // Sorted by path; the nested config is not at the jar root and no metadata
    // names it, so it stays out of the report.
    assert_eq!(
        configs
            .iter()
            .map(|config| config.path.as_str())
            .collect::<Vec<_>>(),
        ["demo.mixins.json", "extra.mixins.json"]
    );
    assert_eq!(
        configs[0],
        MixinConfig {
            path: "demo.mixins.json".into(),
            package: Some("demo.mixin".into()),
            mixins: vec![
                "demo.mixin.AlphaMixin".into(),
                "demo.mixin.beta.GammaMixin".into(),
            ],
            client: vec!["demo.mixin.client.ClientMixin".into()],
            server: vec!["demo.mixin.server.ServerMixin".into()],
            required: false,
            priority: Some(1500),
            mixin_priority: Some(1200),
            plugin: Some("demo.Plugin".into()),
            refmap: Some("demo.refmap.json".into()),
        }
    );
    // An auto-discovered config keeps Mixin's defaults.
    assert!(configs[1].required);
    assert_eq!(configs[1].priority, None);
    assert_eq!(configs[1].mixin_priority, None);
    assert!(jar.mixin_config_errors().is_empty());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn malformed_config_is_reported_not_returned() {
    let (root, jar_path) = build_jar(
        "malformed",
        &[
            (
                "fabric.mod.json",
                br#"{"id":"mixin-config-malformed","mixins":["broken.mixins.json","ok.mixins.json"]}"#,
            ),
            ("broken.mixins.json", b"[]"),
            (
                "ok.mixins.json",
                br#"{"package":"demo.mixin","mixins":["OkMixin"]}"#,
            ),
        ],
    );

    let jar = NativeJar::from_path(&jar_path).unwrap();
    let configs = jar.mixin_configs();
    assert_eq!(
        configs
            .iter()
            .map(|config| config.path.as_str())
            .collect::<Vec<_>>(),
        ["ok.mixins.json"]
    );
    let errors = jar.mixin_config_errors();
    assert_eq!(errors.len(), 1, "unexpected errors: {errors:#?}");
    assert_eq!(errors[0].0, "broken.mixins.json");
    assert!(
        errors[0].1.contains("JSON object"),
        "reason names the shape: {}",
        errors[0].1
    );

    let _ = fs::remove_dir_all(root);
}

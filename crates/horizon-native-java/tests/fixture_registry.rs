use horizon_native_java::{
    Host, MethodRef, NativeJar, RegistryEntry, StdHost, TypedRegistrySink, Value,
};
use std::{fs, io::Write};
use zip::write::SimpleFileOptions;

const ENTRYPOINT_CLASS: &[u8] = &[
    0xca, 0xfe, 0xba, 0xbe, 0x00, 0x00, 0x00, 0x41, 0x00, 0x09, 0x01, 0x00, 0x1a, 0x66, 0x69, 0x78,
    0x74, 0x75, 0x72, 0x65, 0x2f, 0x52, 0x65, 0x67, 0x69, 0x73, 0x74, 0x72, 0x79, 0x45, 0x6e, 0x74,
    0x72, 0x79, 0x70, 0x6f, 0x69, 0x6e, 0x74, 0x07, 0x00, 0x01, 0x01, 0x00, 0x10, 0x6a, 0x61, 0x76,
    0x61, 0x2f, 0x6c, 0x61, 0x6e, 0x67, 0x2f, 0x4f, 0x62, 0x6a, 0x65, 0x63, 0x74, 0x07, 0x00, 0x03,
    0x01, 0x00, 0x06, 0x3c, 0x69, 0x6e, 0x69, 0x74, 0x3e, 0x01, 0x00, 0x0c, 0x6f, 0x6e, 0x49, 0x6e,
    0x69, 0x74, 0x69, 0x61, 0x6c, 0x69, 0x7a, 0x65, 0x01, 0x00, 0x03, 0x28, 0x29, 0x56, 0x01, 0x00,
    0x04, 0x43, 0x6f, 0x64, 0x65, 0x00, 0x21, 0x00, 0x02, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x02, 0x00, 0x01, 0x00, 0x05, 0x00, 0x07, 0x00, 0x01, 0x00, 0x08, 0x00, 0x00, 0x00, 0x0d, 0x00,
    0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0xb1, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x06,
    0x00, 0x07, 0x00, 0x01, 0x00, 0x08, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00,
    0x00, 0x01, 0xb1, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

struct Recording(Vec<RegistryEntry>);

impl TypedRegistrySink for Recording {
    fn register_typed(&mut self, entry: &RegistryEntry) -> Result<(), String> {
        self.0.push(entry.clone());
        Ok(())
    }
}

#[test]
fn fabric_fixture_runs_entrypoint_and_carries_resource_and_registry_declaration() {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-fixture-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let jar_path = root.join("fixture-registry.jar");
    let file = fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default();
    zip.start_file("fabric.mod.json", options).unwrap();
    zip.write_all(br#"{"id":"fixture-registry","version":"1.0","entrypoints":{"main":"fixture.RegistryEntrypoint"}}"#).unwrap();
    zip.start_file("fixture/RegistryEntrypoint.class", options)
        .unwrap();
    zip.write_all(ENTRYPOINT_CLASS).unwrap();
    zip.start_file("assets/fixture-registry/config.json", options)
        .unwrap();
    zip.write_all(br#"{"enabled":true}"#).unwrap();
    zip.start_file("META-INF/horizon-native.json", options)
        .unwrap();
    zip.write_all(br#"{"id":"fixture-registry","registrations":[{"registry":"minecraft:item","id":"fixture:crystal","kind":"item","template":"minecraft:stick"},{"registry":"minecraft:block","id":"fixture:lamp","kind":"block","template":"minecraft:stone","item":false,"properties":{"light":12}},{"registry":"minecraft:mob_effect","id":"fixture:glow","kind":"mob_effect","category":"beneficial","color":16711935}]}"#).unwrap();
    zip.finish().unwrap();

    let jar = NativeJar::from_path(&jar_path).unwrap();
    assert_eq!(jar.fabric_id.as_deref(), Some("fixture-registry"));
    assert_eq!(jar.required_mods, Vec::<String>::new());
    let entry = jar.discover_entrypoint("main").unwrap();
    assert_eq!(entry.class, "fixture.RegistryEntrypoint");
    assert_eq!(jar.native_declarations.len(), 3);
    assert_eq!(
        jar.native_declarations[0].key(),
        "minecraft:item/fixture:crystal"
    );
    assert_eq!(
        jar.native_declarations
            .iter()
            .map(RegistryEntry::key)
            .collect::<Vec<_>>(),
        vec![
            "minecraft:item/fixture:crystal",
            "minecraft:block/fixture:lamp",
            "minecraft:mob_effect/fixture:glow",
        ]
    );

    let mut host = jar
        .seed_host(StdHost::new())
        .with_resource("assets/fixture-registry/config.json", "fixture-config");
    let report = jar
        .execute(&entry.class, entry.method.as_deref(), "main", &mut host)
        .unwrap();
    assert_eq!(report.class, "fixture.RegistryEntrypoint");
    assert!(matches!(
        host.resource("assets/fixture-registry/config.json"),
        Some("fixture-config")
    ));
    let container = host
        .invoke_instance(
            &MethodRef {
                owner: "net/fabricmc/loader/api/FabricLoader".into(),
                name: "getModContainer".into(),
                descriptor: "(Ljava/lang/String;)Ljava/util/Optional;".into(),
            },
            &Value::Ref(Some("fabric-loader".into())),
            &[Value::Ref(Some("fixture-registry".into()))],
        )
        .unwrap();
    let metadata = host
        .invoke_instance(
            &MethodRef {
                owner: "net/fabricmc/loader/api/ModContainer".into(),
                name: "getMetadata".into(),
                descriptor: "()Lnet/fabricmc/loader/api/metadata/ModMetadata;".into(),
            },
            &container,
            &[],
        )
        .unwrap();
    let authors = host
        .invoke_instance(
            &MethodRef {
                owner: "net/fabricmc/loader/api/metadata/ModMetadata".into(),
                name: "getAuthors".into(),
                descriptor: "()Ljava/util/Collection;".into(),
            },
            &metadata,
            &[],
        )
        .unwrap();
    assert_eq!(
        host.invoke_instance(
            &MethodRef {
                owner: "java/util/Collection".into(),
                name: "size".into(),
                descriptor: "()I".into(),
            },
            &authors,
            &[],
        )
        .unwrap(),
        Value::Int(0)
    );
    let contributors = host
        .invoke_instance(
            &MethodRef {
                owner: "net/fabricmc/loader/api/metadata/ModMetadata".into(),
                name: "getContributors".into(),
                descriptor: "()Ljava/util/Collection;".into(),
            },
            &metadata,
            &[],
        )
        .unwrap();
    assert_eq!(
        host.invoke_instance(
            &MethodRef {
                owner: "java/util/Collection".into(),
                name: "size".into(),
                descriptor: "()I".into(),
            },
            &contributors,
            &[],
        )
        .unwrap(),
        Value::Int(0)
    );
    let mut sink = Recording(Vec::new());
    for declaration in &jar.native_declarations {
        sink.register_typed(declaration).unwrap();
    }
    assert_eq!(sink.0.len(), 3);
    assert_eq!(sink.0[0].key(), "minecraft:item/fixture:crystal");
    assert!(matches!(sink.0[0].value, Value::Ref(Some(_))));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn fabric_fixture_rejects_unknown_typed_declaration_kind() {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-invalid-fixture-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let jar_path = root.join("fixture-invalid.jar");
    let file = fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    zip.start_file("META-INF/horizon-native.json", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(br#"{"id":"fixture-invalid","registrations":[{"registry":"minecraft:item","id":"fixture:bad","kind":"not-a-supported-kind"}]}"#).unwrap();
    zip.finish().unwrap();

    let error = NativeJar::from_path(&jar_path).unwrap_err().to_string();
    assert!(
        error.contains("unknown native registration kind not-a-supported-kind"),
        "{error}"
    );

    fs::remove_dir_all(root).unwrap();
}

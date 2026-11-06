//! A jar read from memory (`NativeJar::from_bytes`, what the browser build
//! uses) is the jar read from a file.

use horizon_native_java::NativeJar;
use std::io::Write;
use zip::write::SimpleFileOptions;

const METADATA: &str = r#"{"schemaVersion":1,"id":"bytes_fixture","version":"2.5",
"depends":{"fabric-api":"*"},
"entrypoints":{"main":["demo.Entry"],"client":["demo.ClientEntry"]},
"mixins":["bytes_fixture.mixins.json"]}"#;

const MIXINS: &str = r#"{"package":"demo.mixin","mixins":["StoneMixin"],"client":["ScreenMixin"]}"#;

fn jar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, data) in entries {
        zip.start_file(*name, SimpleFileOptions::default()).unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn summary(jar: &NativeJar) -> String {
    format!(
        "{:?} {:?} {:?} {:?} {:?} {:?}",
        jar.fabric_id,
        jar.fabric_version,
        jar.required_mods,
        jar.discover_entrypoints("main"),
        jar.discover_entrypoints("client"),
        jar.resource_names().collect::<Vec<_>>(),
    ) + &format!(" {:?}", jar.mixin_configs())
}

#[test]
fn a_jar_from_memory_reads_as_the_same_jar_from_a_file() {
    let bytes = jar(&[
        ("fabric.mod.json", METADATA.as_bytes()),
        ("bytes_fixture.mixins.json", MIXINS.as_bytes()),
        ("assets/bytes_fixture/lang/en_us.json", b"{}"),
    ]);
    let path = std::env::temp_dir().join(format!(
        "horizon-native-java-from-bytes-{}.jar",
        std::process::id()
    ));
    std::fs::write(&path, &bytes).unwrap();
    let from_file = NativeJar::from_path(&path).unwrap();
    let from_file_metadata = NativeJar::from_path_metadata(&path).unwrap();
    std::fs::remove_file(&path).unwrap();

    let from_memory = NativeJar::from_bytes(&bytes).unwrap();
    assert_eq!(summary(&from_memory), summary(&from_file));
    assert_eq!(
        summary(&NativeJar::from_bytes_metadata(&bytes).unwrap()),
        summary(&from_file_metadata)
    );
    assert_eq!(from_memory.fabric_id.as_deref(), Some("bytes_fixture"));
    assert_eq!(
        from_memory
            .discover_entrypoint("main")
            .map(|entry| entry.class),
        Some("demo.Entry".to_owned())
    );
}

#[test]
fn the_metadata_read_skips_classes_the_full_read_refuses() {
    // Not a class file: the full read parses it and fails, the metadata read
    // never opens it.
    let bytes = jar(&[
        ("fabric.mod.json", METADATA.as_bytes()),
        ("demo/Entry.class", b"\xCA\xFE\xBA\xBE\x00"),
    ]);
    assert!(NativeJar::from_bytes(&bytes).is_err());
    let metadata = NativeJar::from_bytes_metadata(&bytes).unwrap();
    assert_eq!(metadata.fabric_id.as_deref(), Some("bytes_fixture"));
}

#[test]
fn bytes_that_are_not_a_jar_are_an_error() {
    for bytes in [&b""[..], b"PK\x03\x04", b"not a zip at all"] {
        assert!(NativeJar::from_bytes(bytes).is_err());
        assert!(NativeJar::from_bytes_metadata(bytes).is_err());
    }
}

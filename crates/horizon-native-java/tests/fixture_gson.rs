//! Bounded `com.google.gson` fixtures compiled from real Java at test time.
//!
//! The stubs declare the signatures a config bootstrap compiles against; the
//! interpreter answers them from its own modelled surface (`invoke_gson`).
//! The fixture proves the two directions a real mod needs: JSON text becomes
//! modelled primitives, strings, lists and maps, and those values serialize
//! back. Malformed and over-deep documents are refused with a bounded reason
//! instead of answering an empty value.

use horizon_native_java::{NativeError, NativeJar, StdHost, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};
use zip::write::SimpleFileOptions;

const GSON: &str = r#"package com.google.gson;

public class Gson {
    public Gson() {}
    public String toJson(Object src) { return null; }
    public void toJson(Object src, Appendable writer) {}
    public <T> T fromJson(String json, Class<T> type) { return null; }
    public <T> T fromJson(java.io.Reader reader, Class<T> type) { return null; }
}
"#;

const GSON_BUILDER: &str = r#"package com.google.gson;

public class GsonBuilder {
    public GsonBuilder() {}
    public GsonBuilder setPrettyPrinting() { return this; }
    public Gson create() { return new Gson(); }
}
"#;

const PROBE: &str = r#"package demo;

import com.google.gson.Gson;
import com.google.gson.GsonBuilder;
import java.io.StringReader;
import java.util.List;
import java.util.Map;

public final class Probe {
    public static String roundTrip() {
        Gson gson = new GsonBuilder().setPrettyPrinting().create();
        int number = gson.fromJson("42", Integer.class);
        String text = gson.fromJson("\"hello\"", String.class);
        List<?> items = gson.fromJson("[1,\"two\",[3]]", List.class);
        Map<?, ?> table = gson.fromJson("{\"a\":1,\"b\":\"x\"}", Map.class);
        List<?> reader = gson.fromJson(new StringReader("[7,8]"), List.class);
        StringBuilder sink = new StringBuilder();
        gson.toJson(items, sink);
        return number + "|" + text + "|" + items.size() + "|" + table.size() + "|"
            + reader.size() + "|" + gson.toJson(table) + "|" + sink.toString();
    }

    public static String parse(String json) {
        Gson gson = new GsonBuilder().create();
        List<?> values = gson.fromJson(json, List.class);
        return String.valueOf(values.size());
    }
}
"#;

fn javac() -> PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into())
}

fn files_under(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root)
        .unwrap()
        .flat_map(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files_under(&path)
            } else {
                vec![path]
            }
        })
        .collect()
}

struct Fixture {
    root: PathBuf,
    jar: NativeJar,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    fn build(test: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "horizon-native-java-gson-{test}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let mut fixture = Self {
            root,
            jar: NativeJar::default(),
        };
        let classes = fixture.root.join("classes");
        let sources = [
            ("com/google/gson/Gson.java", GSON),
            ("com/google/gson/GsonBuilder.java", GSON_BUILDER),
            ("demo/Probe.java", PROBE),
        ];
        let mut javac = Command::new(javac());
        javac.args(["--release", "17", "-d"]).arg(&classes);
        for (path, source) in sources {
            let file = fixture.root.join("src").join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, source).unwrap();
            javac.arg(file);
        }
        let status = javac.status().unwrap();
        assert!(status.success(), "javac failed for gson fixture {test}");

        let jar_path = fixture.root.join("gson.jar");
        let mut zip = zip::ZipWriter::new(fs::File::create(&jar_path).unwrap());
        zip.start_file("fabric.mod.json", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(
            br#"{"id":"gson-fixture","version":"1.0","entrypoints":{"main":"demo.Probe"}}"#,
        )
        .unwrap();
        for class in files_under(&classes) {
            let entry = class
                .strip_prefix(&classes)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            zip.start_file(entry, SimpleFileOptions::default()).unwrap();
            zip.write_all(&fs::read(class).unwrap()).unwrap();
        }
        zip.finish().unwrap();

        fixture.jar = NativeJar::from_path(&jar_path).unwrap();
        fixture
    }

    fn call(&self, method: &str, descriptor: &str, args: &[Value]) -> Result<Value, NativeError> {
        self.jar.invoke_method_with_args(
            "demo/Probe",
            method,
            descriptor,
            None,
            args,
            &mut StdHost::new(),
        )
    }

    fn text(&self, method: &str) -> String {
        match self.call(method, "()Ljava/lang/String;", &[]).unwrap() {
            Value::Ref(Some(text)) => text,
            other => panic!("{method} did not return a string: {other:?}"),
        }
    }
}

#[test]
fn gson_round_trips_primitives_strings_lists_and_maps() {
    let fixture = Fixture::build("round-trip");
    assert_eq!(
        fixture.text("roundTrip"),
        "42|hello|3|2|2|{\"a\":1,\"b\":\"x\"}|[1,\"two\",[3]]"
    );
}

#[test]
fn malformed_json_is_refused_with_a_bounded_reason() {
    let fixture = Fixture::build("malformed");
    let error = fixture
        .call(
            "parse",
            "(Ljava/lang/String;)Ljava/lang/String;",
            &[Value::Ref(Some("[".into()))],
        )
        .unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("Gson.fromJson rejected the document"),
        "unexpected error: {message}"
    );
}

#[test]
fn deeply_nested_json_is_refused_by_the_depth_bound() {
    let fixture = Fixture::build("depth");
    let json = format!("{}{}", "[".repeat(40), "]".repeat(40));
    let error = fixture
        .call(
            "parse",
            "(Ljava/lang/String;)Ljava/lang/String;",
            &[Value::Ref(Some(json))],
        )
        .unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("nests") && message.contains("at most 32"),
        "unexpected error: {message}"
    );
}

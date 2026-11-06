//! Fixtures compiled from real Java for the class-model seams a mod
//! bootstrap relies on.
//!
//! Three behaviours are pinned here, each one a shape real jars hit:
//!
//! * `instanceof` walks a jar's own superclass chain and the interfaces a
//!   class implements, so `b instanceof Marker` is true for a subclass marker.
//!   The game classes (`net/minecraft/util/Util`) are compiled against but not
//!   packed, so they exercise the host's own seam.
//! * an inherited field reference names the subclass in its constant pool, and
//!   the read must still find the value the *declaring* class's initializer
//!   stored.
//! * a queue is a collection: `poll`/`peek`/`isEmpty` answer from the same
//!   table a list uses, so `while (!queue.isEmpty())` terminates.

use horizon_native_java::{NativeError, NativeJar, StdHost, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};
use zip::write::SimpleFileOptions;

const UTIL_STUB: &str = r#"package net.minecraft.util;

import java.util.function.Consumer;
import java.util.function.Supplier;

public final class Util {
    public static <T> T make(Supplier<T> factory) { return factory.get(); }
    public static <T> T make(T value, Consumer<T> consumer) {
        consumer.accept(value);
        return value;
    }
}
"#;

const GSON_STUB: &str = r#"package com.google.gson;

public class Gson {
    public Gson() {}
    public <T> T fromJson(String json, Class<T> type) { return null; }
    public String toJson(Object src) { return null; }
}
"#;

const PROBE: &str = r#"package demo;

import java.util.ArrayList;
import java.util.List;
import java.util.PriorityQueue;
import com.google.gson.Gson;
import net.minecraft.util.Util;

public final class Probe {
    public static class Base {
        public static int COUNT = 3;
        public int value;
        public int value() { return value; }
    }

    public static class Derived extends Base {
        public Derived(int value) { this.value = value; }
    }

    public interface Marker {}

    public static final class Tagged extends Derived implements Marker {
        public Tagged() { super(7); }
    }

    public static String inherit() {
        Base tagged = new Tagged();
        Base derived = new Derived(5);
        return (tagged instanceof Marker) + "|" + (tagged instanceof Base) + "|"
            + (tagged instanceof Derived) + "|" + (derived instanceof Marker) + "|"
            + derived.value();
    }

    public static String inheritedStatic() {
        return String.valueOf(Derived.COUNT);
    }

    public static String queue() {
        PriorityQueue<String> queue = new PriorityQueue<>();
        queue.add("a");
        queue.offer("b");
        List<String> seen = new ArrayList<>();
        int guard = 0;
        while (!queue.isEmpty() && guard++ < 10) {
            seen.add(queue.poll());
        }
        return seen.size() + "|" + queue.isEmpty() + "|" + String.valueOf(queue.peek());
    }

    public interface Validator<T> {
        T validate(T value);
    }

    public static final Validator<java.util.Map<String, String>> VALIDATOR =
        value -> {
            java.util.Map<String, String> copy = new java.util.LinkedHashMap<>(value);
            copy.put("validated", "yes");
            return copy;
        };

    public static String customInterfaceLambda() {
        java.util.Map<String, String> source = new java.util.LinkedHashMap<>();
        source.put("key", "value");
        java.util.Map<String, String> validated = VALIDATOR.validate(source);
        return validated.get("key") + "|" + validated.get("validated");
    }

    public static class Config {
        public String name;
        public int size;
        public Nested nested;
        public boolean enabled;
    }

    public static class Nested {
        public String label;
    }

    public static String gsonIntoOwnClass() {
        com.google.gson.Gson gson = new com.google.gson.Gson();
        Config config = gson.fromJson(
            "{\"name\":\"server\",\"size\":7,\"enabled\":true,\"nested\":{\"label\":\"inner\"}}",
            Config.class);
        return config.name + "|" + config.size + "|" + config.enabled + "|" + config.nested.label;
    }

    public static String utilMake() {
        StringBuilder builder = new StringBuilder();
        List<String> made = Util.make(() -> {
            builder.append("supplied");
            return new ArrayList<String>();
        });
        String value = Util.make("item", item -> builder.append("-").append(item));
        return builder.toString() + "|" + made.size() + "|" + value;
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
            "horizon-native-java-class-model-{test}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let mut fixture = Self {
            root,
            jar: NativeJar::default(),
        };
        let classes = fixture.root.join("classes");
        let sources = [
            ("com/google/gson/Gson.java", GSON_STUB),
            ("net/minecraft/util/Util.java", UTIL_STUB),
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
        assert!(
            status.success(),
            "javac failed for class-model fixture {test}"
        );

        let jar_path = fixture.root.join("model.jar");
        let mut zip = zip::ZipWriter::new(fs::File::create(&jar_path).unwrap());
        zip.start_file("fabric.mod.json", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(
            br#"{"id":"class-model-fixture","version":"1.0","entrypoints":{"main":"demo.Probe"}}"#,
        )
        .unwrap();
        for class in files_under(&classes) {
            let entry = class
                .strip_prefix(&classes)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            // The game class stays on the compile path only: the jar under
            // test must reach the host seam, exactly as a real mod does.
            if entry.starts_with("net/minecraft/") {
                continue;
            }
            zip.start_file(entry, SimpleFileOptions::default()).unwrap();
            zip.write_all(&fs::read(class).unwrap()).unwrap();
        }
        zip.finish().unwrap();

        fixture.jar = NativeJar::from_path(&jar_path).unwrap();
        fixture
    }

    fn call(&self, method: &str) -> Result<Value, NativeError> {
        self.jar.invoke_method_with_args(
            "demo/Probe",
            method,
            "()Ljava/lang/String;",
            None,
            &[],
            &mut StdHost::new(),
        )
    }

    fn text(&self, method: &str) -> String {
        match self.call(method).unwrap() {
            Value::Ref(Some(text)) => text,
            other => panic!("{method} did not return a string: {other:?}"),
        }
    }
}

#[test]
fn instanceof_walks_the_jar_superclass_chain_and_interfaces() {
    let fixture = Fixture::build("inherit");
    assert_eq!(fixture.text("inherit"), "true|true|true|false|5");
}

#[test]
fn an_inherited_static_field_reads_the_declaring_class_value() {
    let fixture = Fixture::build("inherited-static");
    assert_eq!(fixture.text("inheritedStatic"), "3");
}

#[test]
fn a_queue_drains_and_reports_empty() {
    let fixture = Fixture::build("queue");
    assert_eq!(fixture.text("queue"), "2|true|null");
}

#[test]
fn util_make_runs_its_supplier_and_consumer() {
    let fixture = Fixture::build("util-make");
    assert_eq!(fixture.text("utilMake"), "supplied-item|0|item");
}

#[test]
fn a_jar_interface_lambda_runs_its_body() {
    let fixture = Fixture::build("custom-interface");
    assert_eq!(fixture.text("customInterfaceLambda"), "value|yes");
}

#[test]
fn gson_materializes_the_jars_own_class_and_injects_fields() {
    let fixture = Fixture::build("gson-class");
    assert_eq!(fixture.text("gsonIntoOwnClass"), "server|7|true|inner");
}

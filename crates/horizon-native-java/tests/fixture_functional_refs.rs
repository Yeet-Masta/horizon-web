//! Callback composition, constructor references and the bootstrap value models
//! that unblocked real corpus jars.
//!
//! Three shapes used to lose the value a call returns, which the next
//! instruction then read off an empty stack:
//!
//! - `Consumer.andThen` on a lambda/method-reference receiver ran the receiver
//!   as a callback and answered `void`, so `builder.batch(consumer.andThen(...))`
//!   passed the builder as the argument (spell_engine).
//! - `Function.apply` on a constructor reference to a class outside the jar
//!   answered the constructor's `void`, so `checkcast` underflowed
//!   (FarmersDelight).
//! - `DataComponentType.builder()` and `Gson.getAdapter` answered `null`, which
//!   `Objects.requireNonNull` refused (FarmersDelight, spell_engine).
//!
//! The fixture compiles the game/Gson stubs into the source tree but leaves
//! them out of the jar, so every one of those calls crosses the host boundary
//! exactly as it does for a real mod. `demo/Probe` is excluded too: the run
//! records its markers in [`StdHost::invocations`].

use horizon_native_java::{NativeJar, StdHost, Value};
use std::{collections::BTreeMap, fs, io::Write, path::Path, process::Command};
use zip::write::SimpleFileOptions;

fn javac() -> std::path::PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| std::path::PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into())
}

fn files_under(root: &Path) -> Vec<std::path::PathBuf> {
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

fn build_jar(sources: BTreeMap<&str, &str>, excluded: &[&str]) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-functional-refs-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    let classes = root.join("classes");
    fs::create_dir_all(&classes).unwrap();
    let mut paths = Vec::new();
    for (relative, body) in sources {
        let path = src.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, body).unwrap();
        paths.push(path);
    }
    let status = Command::new(javac())
        .arg("--release")
        .arg("17")
        .arg("-d")
        .arg(&classes)
        .args(paths)
        .status()
        .unwrap();
    assert!(status.success(), "javac failed");

    let jar_path = root.join("fixture.jar");
    let mut zip = zip::ZipWriter::new(fs::File::create(&jar_path).unwrap());
    zip.start_file("fabric.mod.json", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(
        br#"{"schemaVersion":1,"id":"demo","version":"1","entrypoints":{"main":["demo.Main"]}}"#,
    )
    .unwrap();
    for class in files_under(&classes) {
        let entry = class
            .strip_prefix(&classes)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if excluded.contains(&entry.as_str()) {
            continue;
        }
        zip.start_file(entry, SimpleFileOptions::default()).unwrap();
        zip.write_all(&fs::read(class).unwrap()).unwrap();
    }
    zip.finish().unwrap();
    jar_path
}

fn sources() -> BTreeMap<&'static str, &'static str> {
    let mut sources = BTreeMap::new();
    // A class whose constructor reference must still answer an object even
    // though its bytecode is not in the jar.
    sources.insert(
        "demo/Thing.java",
        r#"package demo;
public class Thing { public Thing(Object source) {} }
"#,
    );
    sources.insert(
        "demo/Probe.java",
        r#"package demo;
public final class Probe {
    public static void mark(String value) {}
    public static void check(String label, boolean value) {}
}
"#,
    );
    // Game and Gson stubs: compiled to type-check Main, excluded from the jar.
    sources.insert(
        "net/minecraft/core/component/DataComponentType.java",
        r#"package net.minecraft.core.component;
public interface DataComponentType<T> {
    static <T> Builder<T> builder() { return null; }
    interface Builder<T> {
        Builder<T> persistent(Object codec);
        Builder<T> networkSynchronized(Object codec);
        Builder<T> cacheEncoding();
        DataComponentType<T> build();
    }
}
"#,
    );
    sources.insert(
        "net/minecraft/core/BlockPos.java",
        r#"package net.minecraft.core;
public class BlockPos {
    public static java.util.stream.Stream<BlockPos> betweenClosedStream(
        int minX, int minY, int minZ, int maxX, int maxY, int maxZ) { return java.util.stream.Stream.empty(); }
}
"#,
    );
    sources.insert(
        "com/google/gson/Gson.java",
        r#"package com.google.gson;
public class Gson {
    public <T> TypeAdapter<T> getAdapter(Class<T> type) { return null; }
}
"#,
    );
    sources.insert(
        "com/google/gson/GsonBuilder.java",
        r#"package com.google.gson;
public class GsonBuilder {
    public Gson create() { return new Gson(); }
    public GsonBuilder setPrettyPrinting() { return this; }
}
"#,
    );
    sources.insert(
        "com/google/gson/TypeAdapter.java",
        r#"package com.google.gson;
public abstract class TypeAdapter<T> {}
"#,
    );
    sources.insert(
        "demo/Main.java",
        r#"package demo;
import com.google.gson.Gson;
import com.google.gson.TypeAdapter;
import java.util.List;
import java.util.Objects;
import java.util.function.Consumer;
import java.util.function.Function;
import java.util.function.Supplier;
import net.minecraft.core.BlockPos;
import net.minecraft.core.component.DataComponentType;

public final class Main {
    static final class Builder {
        final StringBuilder order = new StringBuilder();
        Builder batch(Consumer<Builder> consumer) {
            consumer.accept(this);
            return this;
        }
    }

    public static void main() {
        // Consumer.andThen: both callbacks must run, in order, with the real
        // argument. Before the fix the second consumer was passed as `mark`'s
        // argument and only one callback ran.
        Consumer<String> first = s -> Probe.mark("first:" + s);
        Consumer<String> second = s -> Probe.mark("second:" + s);
        first.andThen(second).accept("value");

        // Function.apply on a constructor reference to a class outside the jar.
        Function<Object, Thing> factory = Thing::new;
        Object made = factory.apply("unused");
        Probe.check("constructor-ref", made != null);

        // DataComponentType.builder() must be a real value, and a fluent
        // setter must answer the builder (`Objects.requireNonNull` refuses null).
        DataComponentType.Builder<Object> builder = DataComponentType.builder();
        Probe.check("component-builder", builder != null);
        builder = builder.persistent("codec").networkSynchronized("stream").cacheEncoding();
        Probe.check("component-value", builder.build() != null);

        // A codec bootstrap asks Gson for an adapter and checks it is present.
        TypeAdapter<String> adapter = new Gson().getAdapter(String.class);
        Probe.check("gson-adapter", adapter != null);

        // The game's bounding-box stream is a stream the interpreter can filter
        // and collect, even when it yields no positions.
        List<BlockPos> offsets = BlockPos.betweenClosedStream(-1, 0, -1, 1, 1, 1)
            .filter(pos -> true)
            .toList();
        Probe.check("position-stream", offsets.isEmpty());
    }
}
"#,
    );
    sources
}

/// The `Probe` markers the run recorded, in order, and each check's value.
fn probe_calls(host: &StdHost) -> (Vec<String>, BTreeMap<String, bool>) {
    let mut marks = Vec::new();
    let mut checks = BTreeMap::new();
    for invocation in host.invocations() {
        if invocation.method.owner != "demo/Probe" {
            continue;
        }
        match (invocation.method.name.as_str(), invocation.args.as_slice()) {
            ("mark", [Value::Ref(Some(value))]) => marks.push(value.clone()),
            ("check", [Value::Ref(Some(label)), Value::Int(flag)]) => {
                checks.insert(label.clone(), *flag != 0);
            }
            _ => {}
        }
    }
    (marks, checks)
}

#[test]
fn composition_and_constructor_references_keep_their_values() {
    let jar_path = build_jar(
        sources(),
        &[
            "demo/Probe.class",
            "demo/Thing.class",
            "net/minecraft/core/BlockPos.class",
            "net/minecraft/core/component/DataComponentType.class",
            "net/minecraft/core/component/DataComponentType$Builder.class",
            "com/google/gson/Gson.class",
            "com/google/gson/GsonBuilder.class",
            "com/google/gson/TypeAdapter.class",
        ],
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();
    let mut host = StdHost::new();
    jar.execute("demo.Main", Some("main"), "main", &mut host)
        .unwrap();

    let (marks, checks) = probe_calls(&host);
    assert_eq!(marks, ["first:value", "second:value"]);
    assert_eq!(
        checks,
        BTreeMap::from([
            ("component-builder".to_owned(), true),
            ("component-value".to_owned(), true),
            ("constructor-ref".to_owned(), true),
            ("gson-adapter".to_owned(), true),
            ("position-stream".to_owned(), true),
        ])
    );
}

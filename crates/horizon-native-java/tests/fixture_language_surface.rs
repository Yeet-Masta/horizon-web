//! Source-built Fabric fixtures for the bounded runtime's string surface:
//! `StringBuilder` mutation and `StringConcatFactory` call sites.

use horizon_native_java::{ExecutionReport, NativeError, NativeJar, StdHost};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};
use zip::write::SimpleFileOptions;

/// Compile one entrypoint and package it as a Fabric jar, the same way the
/// other source-built fixtures do. Each fixture gets its own directory so the
/// tests never share a build root.
fn build_fixture(tag: &str, id: &str, entrypoint: &str, source: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-language-{tag}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let class = entrypoint.replace('.', "/");
    let source_path = root.join("src").join(format!("{class}.java"));
    fs::create_dir_all(source_path.parent().unwrap()).unwrap();
    fs::create_dir_all(root.join("classes")).unwrap();
    fs::write(&source_path, source).unwrap();

    let javac = std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into());
    let status = Command::new(javac)
        .arg("--release")
        .arg("17")
        .arg("-d")
        .arg(root.join("classes"))
        .arg(&source_path)
        .status()
        .unwrap();
    assert!(status.success(), "javac failed for {entrypoint}");

    let jar_path = root.join(format!("{id}.jar"));
    let file = fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default();
    zip.start_file("fabric.mod.json", options).unwrap();
    zip.write_all(
        format!(r#"{{"id":"{id}","version":"1.0","entrypoints":{{"main":"{entrypoint}"}}}}"#)
            .as_bytes(),
    )
    .unwrap();
    zip.start_file(format!("{class}.class"), options).unwrap();
    zip.write_all(&fs::read(root.join("classes").join(format!("{class}.class"))).unwrap())
        .unwrap();
    zip.finish().unwrap();
    jar_path
}

fn execute(jar_path: &Path) -> Result<ExecutionReport, NativeError> {
    let jar = NativeJar::from_path(jar_path).unwrap();
    let entry = jar.discover_entrypoint("main").unwrap();
    let mut host = StdHost::new();
    jar.execute(&entry.class, entry.method.as_deref(), "main", &mut host)
}

#[test]
fn appends_mutate_the_builder_and_concat_sites_produce_their_text() {
    let jar = build_fixture(
        "strings",
        "fixture-strings",
        "demo.StringsEntrypoint",
        r#"
package demo;
public final class StringsEntrypoint {
  public void onInitialize() {
    StringBuilder builder = new StringBuilder();
    builder.append("a");
    builder.append(42);
    builder.append('!');
    if (!builder.toString().equals("a42!")) {
      throw new IllegalStateException("builder=" + builder.toString());
    }
    StringBuilder seeded = new StringBuilder("seed");
    seeded.append("-tail");
    if (seeded.length() != 9) {
      throw new IllegalStateException("length=" + seeded.length());
    }
    String joined = "x" + 42 + "y";
    if (!joined.equals("x42y")) {
      throw new IllegalStateException("joined=" + joined);
    }
    StringBuilder reset = new StringBuilder("discard me");
    reset.setLength(0);
    reset.append("kept");
    if (!reset.toString().equals("kept")) {
      throw new IllegalStateException("reset=" + reset.toString());
    }
  }
}
"#,
    );

    let report = execute(&jar).expect("the string surface must run through the entrypoint");
    assert_eq!(report.class, "demo.StringsEntrypoint");
}

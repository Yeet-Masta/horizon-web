//! `String.split` answers a runtime-owned `String[]`.
//!
//! The host layer has no array table, so an unmodelled split used to yield
//! `null`, and the caller's `arraylength` then read as
//! `Metadata("expected a native array marker")`. This fixture keeps the shape a
//! real config writer uses (split a serialized object, walk the lines) and
//! checks the parts are addressable rather than reporting that misleading
//! error.

use horizon_native_java::{NativeJar, StdHost, Value};
use std::{fs, io::Write, path::PathBuf, process::Command};
use zip::write::SimpleFileOptions;

const SOURCE: &str = r#"
package demo;
public final class SplitEntrypoint {
  public void onInitialize() {
    String[] lines = "{\r\n  \"a\": 1,\n  \"b\": 2\r\n}".split("\\R");
    if (lines.length != 4) {
      throw new IllegalStateException("split-length");
    }
    if (!lines[0].equals("{")
        || !lines[1].trim().equals("\"a\": 1,")
        || !lines[2].trim().equals("\"b\": 2")
        || !lines[3].equals("}")) {
      throw new IllegalStateException("parts");
    }
    String[] limited = "a,b,c".split(",", 2);
    if (limited.length != 2 || !limited[1].equals("b,c")) {
      throw new IllegalStateException("limit-shape");
    }
    String[] trailing = "a,b,,".split(",");
    if (trailing.length != 2 || !trailing[0].equals("a")) {
      throw new IllegalStateException("trailing-shape");
    }
    Probe.mark("split", lines.length);
  }
}
"#;

const PROBE: &str = r#"package demo;
public final class Probe {
    public static void mark(String marker, int value) {}
}
"#;

fn build_fixture() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-string-split-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let source_path = root.join("src/demo/SplitEntrypoint.java");
    let probe_path = root.join("src/demo/Probe.java");
    fs::create_dir_all(source_path.parent().unwrap()).unwrap();
    fs::create_dir_all(root.join("classes")).unwrap();
    fs::write(&source_path, SOURCE).unwrap();
    fs::write(&probe_path, PROBE).unwrap();
    let javac = std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into());
    let status = Command::new(javac)
        .args(["--release", "17", "-d"])
        .arg(root.join("classes"))
        .arg(&source_path)
        .arg(&probe_path)
        .status()
        .unwrap();
    assert!(status.success(), "javac failed");

    let jar_path = root.join("fixture-string-split.jar");
    let file = fs::File::create(&jar_path).unwrap();
    let mut jar = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default();
    jar.start_file("fabric.mod.json", options).unwrap();
    jar.write_all(
        br#"{"id":"fixture-string-split","version":"1.0","entrypoints":{"main":"demo.SplitEntrypoint"}}"#,
    )
    .unwrap();
    // `demo/Probe` stays out of the jar on purpose: its call crosses the host
    // boundary, which records it.
    jar.start_file("demo/SplitEntrypoint.class", options)
        .unwrap();
    jar.write_all(&fs::read(root.join("classes/demo/SplitEntrypoint.class")).unwrap())
        .unwrap();
    jar.finish().unwrap();
    jar_path
}

#[test]
fn split_parts_are_runtime_owned_arrays() {
    let path = build_fixture();
    let jar = NativeJar::from_path(&path).unwrap();
    let mut host = jar.seed_host(StdHost::new());
    jar.execute("demo/SplitEntrypoint", None, "main", &mut host)
        .unwrap();
    let marks = host
        .invocations()
        .iter()
        .filter(|invocation| invocation.method.owner == "demo/Probe")
        .map(|invocation| match invocation.args.as_slice() {
            [Value::Ref(Some(marker)), Value::Int(value)] => (marker.clone(), *value),
            other => panic!("unexpected Probe.mark arguments: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(marks, [("split".to_owned(), 4)]);
}

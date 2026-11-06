//! A source-built Fabric fixture whose entrypoint throws, proving that an
//! uncaught Java exception fails native execution with a named diagnostic
//! instead of reporting success.

use horizon_native_java::{NativeError, NativeJar, StdHost};
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
        "horizon-native-java-exception-{tag}-{}",
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

fn execute(jar_path: &Path) -> Result<horizon_native_java::ExecutionReport, NativeError> {
    let jar = NativeJar::from_path(jar_path).unwrap();
    let entry = jar.discover_entrypoint("main").unwrap();
    let mut host = StdHost::new();
    jar.execute(&entry.class, entry.method.as_deref(), "main", &mut host)
}

#[test]
fn uncaught_illegal_state_exception_fails_native_execution() {
    let jar = build_fixture(
        "throwing",
        "fixture-throwing",
        "demo.ThrowingEntrypoint",
        r#"
package demo;
public final class ThrowingEntrypoint {
  public void onInitialize() {
    throw new IllegalStateException("boom");
  }
}
"#,
    );

    let error = execute(&jar).expect_err("an uncaught throw must fail the entrypoint");
    let text = error.to_string();
    assert!(
        text.contains("IllegalStateException") && text.contains("boom"),
        "diagnostic names neither the class nor the message: {text}"
    );
    match error {
        NativeError::UncaughtException { class, message, .. } => {
            assert_eq!(class, "java/lang/IllegalStateException");
            assert_eq!(message.as_deref(), Some("boom"));
        }
        other => panic!("expected an uncaught exception, got {other:?}"),
    }
}

#[test]
fn constructed_but_unthrown_exception_still_executes() {
    let jar = build_fixture(
        "quiet",
        "fixture-quiet",
        "demo.QuietEntrypoint",
        r#"
package demo;
public final class QuietEntrypoint {
  public void onInitialize() {
    IllegalStateException error = new IllegalStateException("never thrown");
    error.getMessage();
  }
}
"#,
    );

    let report = execute(&jar).expect("constructing an exception must not fail the entrypoint");
    assert_eq!(report.class, "demo.QuietEntrypoint");
    assert_eq!(report.method, "onInitialize");
}

#[test]
fn caught_null_pointer_from_require_non_null_continues_the_entrypoint() {
    let jar = build_fixture(
        "caught-null",
        "fixture-caught-null",
        "demo.CaughtNullEntrypoint",
        r#"
package demo;
public final class CaughtNullEntrypoint {
  public void onInitialize() {
    String text;
    try {
      text = java.util.Objects.requireNonNull((String) null, "value");
    } catch (NullPointerException expected) {
      text = "caught";
    }
    if (!"caught".equals(text)) {
      throw new IllegalStateException("not caught: " + text);
    }
  }
}
"#,
    );

    let report = execute(&jar).expect("a caught NullPointerException must not fail the entrypoint");
    assert_eq!(report.class, "demo.CaughtNullEntrypoint");
}

#[test]
fn a_nested_method_throw_is_caught_by_the_caller() {
    let jar = build_fixture(
        "nested-catch",
        "fixture-nested-catch",
        "demo.NestedCatchEntrypoint",
        r#"
package demo;
public final class NestedCatchEntrypoint {
  private static void fail() {
    throw new IllegalStateException("inner");
  }

  public void onInitialize() {
    try {
      fail();
    } catch (IllegalStateException expected) {
      if (!"inner".equals(expected.getMessage())) {
        throw new IllegalStateException("message: " + expected.getMessage());
      }
    }
  }
}
"#,
    );

    let report = execute(&jar).expect("the caller's exception table must catch the nested throw");
    assert_eq!(report.class, "demo.NestedCatchEntrypoint");
}

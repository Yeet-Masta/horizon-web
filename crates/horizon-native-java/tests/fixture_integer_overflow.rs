//! Java's signed division has one defined overflow result that Rust's checked
//! arithmetic rejects: `MIN_VALUE / -1` wraps to `MIN_VALUE`, with remainder 0.

use horizon_native_java::{NativeJar, StdHost, Value};
use std::{fs, io::Write, path::PathBuf, process::Command};
use zip::write::SimpleFileOptions;

const SOURCE: &str = r#"
package demo;
public final class IntegerOverflowEntrypoint {
  public static int intDivide(int value) { return value / -1; }
  public static int intRemainder(int value) { return value % -1; }
  public static long longDivide(long value) { return value / -1L; }
  public static long longRemainder(long value) { return value % -1L; }
  public void onInitialize() {
    if (intDivide(Integer.MIN_VALUE) != Integer.MIN_VALUE
        || intRemainder(Integer.MIN_VALUE) != 0
        || longDivide(Long.MIN_VALUE) != Long.MIN_VALUE
        || longRemainder(Long.MIN_VALUE) != 0L) {
      throw new IllegalStateException("signed division overflow semantics changed");
    }
  }
}
"#;

fn build_fixture() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-integer-overflow-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let source_path = root.join("src/demo/IntegerOverflowEntrypoint.java");
    fs::create_dir_all(source_path.parent().unwrap()).unwrap();
    fs::create_dir_all(root.join("classes")).unwrap();
    fs::write(&source_path, SOURCE).unwrap();
    let javac = std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into());
    let status = Command::new(javac)
        .args(["--release", "17", "-d"])
        .arg(root.join("classes"))
        .arg(&source_path)
        .status()
        .unwrap();
    assert!(status.success(), "javac failed");

    let jar_path = root.join("fixture-integer-overflow.jar");
    let file = fs::File::create(&jar_path).unwrap();
    let mut jar = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default();
    jar.start_file("fabric.mod.json", options).unwrap();
    jar.write_all(
        br#"{"id":"fixture-integer-overflow","version":"1.0","entrypoints":{"main":"demo.IntegerOverflowEntrypoint"}}"#,
    )
    .unwrap();
    jar.start_file("demo/IntegerOverflowEntrypoint.class", options)
        .unwrap();
    jar.write_all(&fs::read(root.join("classes/demo/IntegerOverflowEntrypoint.class")).unwrap())
        .unwrap();
    jar.finish().unwrap();
    jar_path
}

fn call(jar: &NativeJar, name: &str, descriptor: &str, args: &[Value]) -> Value {
    jar.invoke_method_with_args(
        "demo/IntegerOverflowEntrypoint",
        name,
        descriptor,
        None,
        args,
        &mut StdHost::new(),
    )
    .unwrap()
}

#[test]
fn signed_division_overflow_matches_java() {
    let fixture = build_fixture();
    let jar = NativeJar::from_path(&fixture).unwrap();
    assert_eq!(
        call(&jar, "intDivide", "(I)I", &[Value::Int(i32::MIN)]),
        Value::Int(i32::MIN)
    );
    assert_eq!(
        call(&jar, "intRemainder", "(I)I", &[Value::Int(i32::MIN)]),
        Value::Int(0)
    );
    assert_eq!(
        call(&jar, "longDivide", "(J)J", &[Value::Long(i64::MIN)]),
        Value::Long(i64::MIN)
    );
    assert_eq!(
        call(&jar, "longRemainder", "(J)J", &[Value::Long(i64::MIN)]),
        Value::Long(0)
    );

    let entry = jar.discover_entrypoint("main").unwrap();
    jar.execute(
        &entry.class,
        entry.method.as_deref(),
        "main",
        &mut StdHost::new(),
    )
    .unwrap();
}

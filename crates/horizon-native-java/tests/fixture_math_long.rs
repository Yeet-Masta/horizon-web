//! Source-built coverage for the long overloads of the bounded `Math` host.

use horizon_native_java::{NativeError, NativeJar, StdHost, Value};
use std::{fs, io::Write, path::PathBuf, process::Command};
use zip::write::SimpleFileOptions;

const SOURCE: &str = r#"
package demo;
public final class MathLongEntrypoint {
  public static int absoluteInt(int value) { return Math.abs(value); }
  public static long absolute(long value) { return Math.abs(value); }
  public static long maximum(long left, long right) { return Math.max(left, right); }
  public static long minimum(long left, long right) { return Math.min(left, right); }
  public static long floorDivide(long left, long right) { return Math.floorDiv(left, right); }
  public static long floorModulo(long left, long right) { return Math.floorMod(left, right); }
  public static long exactAdd(long left, long right) { return Math.addExact(left, right); }
  public void onInitialize() {
    if (absolute(Long.MIN_VALUE) != Long.MIN_VALUE
        || floorDivide(-7L, 3L) != -3L
        || floorModulo(-7L, 3L) != 2L) {
      throw new IllegalStateException("long Math semantics changed");
    }
  }
}
"#;

fn build_fixture() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-math-long-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let source_path = root.join("src/demo/MathLongEntrypoint.java");
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

    let jar_path = root.join("fixture-math-long.jar");
    let file = fs::File::create(&jar_path).unwrap();
    let mut jar = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default();
    jar.start_file("fabric.mod.json", options).unwrap();
    jar.write_all(
        br#"{"id":"fixture-math-long","version":"1.0","entrypoints":{"main":"demo.MathLongEntrypoint"}}"#,
    )
    .unwrap();
    jar.start_file("demo/MathLongEntrypoint.class", options)
        .unwrap();
    jar.write_all(&fs::read(root.join("classes/demo/MathLongEntrypoint.class")).unwrap())
        .unwrap();
    jar.finish().unwrap();
    jar_path
}

fn call(
    jar: &NativeJar,
    name: &str,
    descriptor: &str,
    args: &[Value],
) -> Result<Value, NativeError> {
    jar.invoke_method_with_args(
        "demo/MathLongEntrypoint",
        name,
        descriptor,
        None,
        args,
        &mut StdHost::new(),
    )
}

#[test]
fn long_math_overloads_match_java() {
    let fixture = build_fixture();
    let jar = NativeJar::from_path(&fixture).unwrap();
    assert_eq!(
        call(&jar, "absoluteInt", "(I)I", &[Value::Int(i32::MIN)]).unwrap(),
        Value::Int(i32::MIN)
    );
    assert_eq!(
        call(&jar, "absolute", "(J)J", &[Value::Long(i64::MIN)]).unwrap(),
        Value::Long(i64::MIN)
    );
    assert_eq!(
        call(&jar, "maximum", "(JJ)J", &[Value::Long(-9), Value::Long(4)],).unwrap(),
        Value::Long(4)
    );
    assert_eq!(
        call(&jar, "minimum", "(JJ)J", &[Value::Long(-9), Value::Long(4)],).unwrap(),
        Value::Long(-9)
    );
    assert_eq!(
        call(
            &jar,
            "floorDivide",
            "(JJ)J",
            &[Value::Long(-7), Value::Long(3)],
        )
        .unwrap(),
        Value::Long(-3)
    );
    assert_eq!(
        call(
            &jar,
            "floorModulo",
            "(JJ)J",
            &[Value::Long(-7), Value::Long(3)],
        )
        .unwrap(),
        Value::Long(2)
    );
    assert_eq!(
        call(
            &jar,
            "floorDivide",
            "(JJ)J",
            &[Value::Long(i64::MIN), Value::Long(-1)],
        )
        .unwrap(),
        Value::Long(i64::MIN)
    );
    assert_eq!(
        call(
            &jar,
            "floorModulo",
            "(JJ)J",
            &[Value::Long(i64::MIN), Value::Long(-1)],
        )
        .unwrap(),
        Value::Long(0)
    );
    assert_eq!(
        call(
            &jar,
            "exactAdd",
            "(JJ)J",
            &[Value::Long(12), Value::Long(30)],
        )
        .unwrap(),
        Value::Long(42)
    );
    assert!(matches!(
        call(
            &jar,
            "exactAdd",
            "(JJ)J",
            &[Value::Long(i64::MAX), Value::Long(1)],
        ),
        Err(NativeError::Arithmetic { .. })
    ));

    let entry = jar.discover_entrypoint("main").unwrap();
    jar.execute(
        &entry.class,
        entry.method.as_deref(),
        "main",
        &mut StdHost::new(),
    )
    .unwrap();
}

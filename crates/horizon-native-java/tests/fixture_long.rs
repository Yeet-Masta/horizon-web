//! A source-built Fabric fixture that exercises the exact `long` model.
//!
//! The bounded interpreter used to widen `long` through `i32`, so `long` ids,
//! timers and arithmetic were wrong past 2^31. Each method here pins one
//! opcode family to a value that only a real 64-bit path can produce: the
//! `ldc2_w` constant, `ladd`/`lsub`/`lmul`/`ldiv`/`lrem`/`lneg`, the shifts,
//! `lcmp`, `i2l`/`l2i`, a long local, a `long[]` round trip, and the
//! `Long.valueOf`/`parseLong`/`toString`/`compare` host surface.

use horizon_native_java::{NativeJar, StdHost, Value};
use std::{fs, io::Write, path::PathBuf, process::Command};
use zip::write::SimpleFileOptions;

/// The javac build root this fixture compiles in. Each test gets its own tag
/// so the parallel test threads never share a build root.
fn build_root(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "horizon-native-java-long-{tag}-{}",
        std::process::id()
    ))
}

fn javac() -> PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into())
}

/// Compile the fixture class and package it as a Fabric jar.
fn build_fixture(tag: &str, id: &str, entrypoint: &str, source: &str) -> PathBuf {
    let root = build_root(tag);
    let _ = fs::remove_dir_all(&root);
    let class = entrypoint.replace('.', "/");
    let source_path = root.join("src").join(format!("{class}.java"));
    fs::create_dir_all(source_path.parent().unwrap()).unwrap();
    fs::create_dir_all(root.join("classes")).unwrap();
    fs::write(&source_path, source).unwrap();

    let status = Command::new(javac())
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

const SOURCE: &str = r#"
package demo;

public final class LongEntrypoint {
  public static long maxMinusOne() {
    return Long.MAX_VALUE - 1L;
  }

  public static long constant() {
    return 4294967297L;
  }

  public static long negate(long value) {
    return -value;
  }

  public static long shifted(long value, int count) {
    return (value << count) | (value >>> count);
  }

  public static long arithmeticShift(long value, int count) {
    return value >> count;
  }

  public static int compare(long left, long right) {
    return Long.compare(left, right);
  }

  public static int rawCompare(long left, long right) {
    if (left < right) {
      return -1;
    }
    return left == right ? 0 : 1;
  }

  public static long parsed(String text) {
    return Long.parseLong(text);
  }

  public static String printed(long value) {
    return Long.toString(value);
  }

  public static long localRoundTrip(long value) {
    long doubled = value * 2L + 1L;
    long remainder = doubled % 1000L;
    return doubled / 2L + remainder;
  }

  public static long arrayRoundTrip(long value) {
    long[] values = new long[3];
    values[0] = value;
    values[1] = values[0] + 5L;
    values[2] = values[1] * 3L;
    return values[2];
  }

  public static long boxed(long value) {
    Long boxed = value;
    return boxed + 1L;
  }

  public static long truncate(long value) {
    int narrowed = (int) value;
    return (long) narrowed;
  }

  public static long scale(int factor, long value) {
    return value * factor;
  }

  public void onInitialize() {
    long probe = maxMinusOne();
    if (probe < 0L) {
      throw new IllegalStateException("long was truncated to 32 bits");
    }
  }
}
"#;

fn jar(tag: &str) -> NativeJar {
    NativeJar::from_path(&build_fixture(
        tag,
        "fixture-long",
        "demo.LongEntrypoint",
        SOURCE,
    ))
    .unwrap()
}

/// Call a static fixture method and return its value.
fn call(jar: &NativeJar, name: &str, descriptor: &str, args: &[Value]) -> Value {
    jar.invoke_method_with_args(
        "demo.LongEntrypoint",
        name,
        descriptor,
        None,
        args,
        &mut StdHost::new(),
    )
    .unwrap_or_else(|error| panic!("{name}{descriptor} failed: {error}"))
}

#[test]
fn long_entrypoint_executes_without_truncation() {
    let jar = jar("entrypoint");
    let entry = jar.discover_entrypoint("main").unwrap();
    let report = jar
        .execute(
            &entry.class,
            entry.method.as_deref(),
            "main",
            &mut StdHost::new(),
        )
        .unwrap();
    assert_eq!(report.class, "demo.LongEntrypoint");
    assert_eq!(report.method, "onInitialize");
}

#[test]
fn long_constants_and_arithmetic_keep_all_64_bits() {
    let jar = jar("arithmetic");
    assert_eq!(
        call(&jar, "maxMinusOne", "()J", &[]),
        Value::Long(i64::MAX - 1)
    );
    // 2^32 + 1 only survives through `ldc2_w`.
    assert_eq!(
        call(&jar, "constant", "()J", &[]),
        Value::Long(4_294_967_297)
    );
    assert_eq!(
        call(&jar, "negate", "(J)J", &[Value::Long(4_294_967_297)]),
        Value::Long(-4_294_967_297)
    );
    assert_eq!(
        call(&jar, "negate", "(J)J", &[Value::Long(i64::MIN)]),
        Value::Long(i64::MIN)
    );
}

#[test]
fn long_shifts_and_compare_use_the_wide_value() {
    let jar = jar("shifts");
    assert_eq!(
        call(
            &jar,
            "shifted",
            "(JI)J",
            &[Value::Long(0x4000_0000_0000_0001), Value::Int(4)]
        ),
        Value::Long(
            (0x4000_0000_0000_0001i64 << 4) | ((0x4000_0000_0000_0001i64 as u64 >> 4) as i64)
        )
    );
    assert_eq!(
        call(
            &jar,
            "arithmeticShift",
            "(JI)J",
            &[Value::Long(-8_589_934_592), Value::Int(33)]
        ),
        Value::Long(-8_589_934_592i64 >> 33)
    );
    assert_eq!(
        call(
            &jar,
            "compare",
            "(JJ)I",
            &[Value::Long(-4_294_967_297), Value::Long(4_294_967_297)]
        ),
        Value::Int(-1)
    );
    assert_eq!(
        call(
            &jar,
            "compare",
            "(JJ)I",
            &[Value::Long(i64::MAX), Value::Long(i64::MAX)]
        ),
        Value::Int(0)
    );
    assert_eq!(
        call(
            &jar,
            "rawCompare",
            "(JJ)I",
            &[Value::Long(4_294_967_297), Value::Long(-4_294_967_297)]
        ),
        Value::Int(1)
    );
}

#[test]
fn long_parse_and_print_round_trip_exact_text() {
    let jar = jar("text");
    let parsed = call(
        &jar,
        "parsed",
        "(Ljava/lang/String;)J",
        &[Value::Ref(Some("9223372036854775806".into()))],
    );
    assert_eq!(parsed, Value::Long(9_223_372_036_854_775_806));
    assert_eq!(
        call(
            &jar,
            "printed",
            "(J)Ljava/lang/String;",
            &[Value::Long(9_223_372_036_854_775_806)]
        ),
        Value::Ref(Some("9223372036854775806".into()))
    );
    assert_eq!(
        call(
            &jar,
            "parsed",
            "(Ljava/lang/String;)J",
            &[Value::Ref(Some("-4294967297".into()))]
        ),
        Value::Long(-4_294_967_297)
    );
}

#[test]
fn long_locals_arrays_and_boxing_round_trip() {
    let jar = jar("locals");
    // (v * 2 + 1) / 2 + (v * 2 + 1) % 1000
    let value = 4_294_967_297i64;
    let doubled = value * 2 + 1;
    assert_eq!(
        call(&jar, "localRoundTrip", "(J)J", &[Value::Long(value)]),
        Value::Long(doubled / 2 + doubled % 1000)
    );
    assert_eq!(
        call(&jar, "arrayRoundTrip", "(J)J", &[Value::Long(value)]),
        Value::Long((value + 5) * 3)
    );
    assert_eq!(
        call(&jar, "boxed", "(J)J", &[Value::Long(value)]),
        Value::Long(value + 1)
    );
    // l2i keeps the low 32 bits, i2l widens them back.
    assert_eq!(
        call(&jar, "truncate", "(J)J", &[Value::Long(value)]),
        Value::Long(1)
    );
    assert_eq!(
        call(
            &jar,
            "scale",
            "(IJ)J",
            &[Value::Int(3), Value::Long(4_294_967_297)]
        ),
        Value::Long(12_884_901_891)
    );
}

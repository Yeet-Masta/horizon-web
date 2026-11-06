//! A source-built Fabric fixture that exercises the exact `float`/`double` model.
//!
//! The bounded interpreter used to neutralise floating point through `i32`, so a
//! `double` attribute (`0.1 + 0.2`, `Double.MAX_VALUE / 2`) lost every bit after
//! the decimal point. Each method here pins one opcode family to a value only a
//! real `f32`/`f64` path can produce: the `ldc`/`ldc2_w` constants, the
//! `dadd`/`ddiv`/`drem`/`dneg` arithmetic, `f` rounding, `fcmpl`/`dcmpg` (with
//! `NaN`), every numeric conversion, a `double` local and array, `freturn`/
//! `dreturn`, and the `Double`/`Float`/`String.valueOf`/`StringBuilder` surface.

use horizon_native_java::{NativeJar, StdHost, Value};
use std::{fs, io::Write, path::PathBuf, process::Command};
use zip::write::SimpleFileOptions;

/// The javac build root this fixture compiles in. Each test gets its own tag
/// so the parallel test threads never share a build root.
fn build_root(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "horizon-native-java-float-{tag}-{}",
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

public final class FloatEntrypoint {
  // Constant expressions: javac folds these into one `ldc2_w`, so they pin the
  // constant pool rather than the arithmetic opcodes.
  public static double foldedSum() {
    return 0.1 + 0.2;
  }

  public static double foldedHalfMax() {
    return Double.MAX_VALUE / 2.0;
  }

  public static double maxValue() {
    return Double.MAX_VALUE;
  }

  public static float maxFloat() {
    return Float.MAX_VALUE;
  }

  // Parameters force the real opcodes.
  public static double add(double left, double right) {
    return left + right;
  }

  public static double subtract(double left, double right) {
    return left - right;
  }

  public static double multiply(double left, double right) {
    return left * right;
  }

  public static double divide(double left, double right) {
    return left / right;
  }

  public static double remainder(double left, double right) {
    return left % right;
  }

  public static double negate(double value) {
    return -value;
  }

  public static float addFloat(float left, float right) {
    return left + right;
  }

  public static float divFloat(float left, float right) {
    return left / right;
  }

  public static float negateFloat(float value) {
    return -value;
  }

  // Conversions in both directions and both widths.
  public static double widenFloat(float value) {
    return value;
  }

  public static float narrowDouble(double value) {
    return (float) value;
  }

  public static double widenLong(long value) {
    return value;
  }

  public static float longToFloat(long value) {
    return value;
  }

  public static float intToFloat(int value) {
    return value;
  }

  public static double intToDouble(int value) {
    return value;
  }

  public static int truncateDouble(double value) {
    return (int) value;
  }

  public static long truncateLong(double value) {
    return (long) value;
  }

  public static int truncateFloat(float value) {
    return (int) value;
  }

  public static long truncateFloatLong(float value) {
    return (long) value;
  }

  // Comparisons: `<` compiles to `cmpg` and `>` to `cmpl`, so a NaN operand
  // answers false for both.
  public static int compare(double left, double right) {
    if (left < right) {
      return -1;
    }
    if (left > right) {
      return 1;
    }
    return 0;
  }

  public static int compareFloat(float left, float right) {
    if (left < right) {
      return -1;
    }
    if (left > right) {
      return 1;
    }
    return 0;
  }

  public static boolean less(double left, double right) {
    return left < right;
  }

  public static boolean greater(double left, double right) {
    return left > right;
  }

  public static boolean same(double value) {
    return value == value;
  }

  public static boolean lessFloat(float left, float right) {
    return left < right;
  }

  public static boolean greaterFloat(float left, float right) {
    return left > right;
  }

  public static double nan() {
    return 0.0 / 0.0;
  }

  public static boolean isNaN(double value) {
    return Double.isNaN(value);
  }

  public static boolean isInfinite(double value) {
    return Double.isInfinite(value);
  }

  public static int compareTotal(double left, double right) {
    return Double.compare(left, right);
  }

  // Locals, arrays and the default element value.
  public static double localRoundTrip(double value) {
    double half = value / 2.0;
    double scaled = half * 3.0;
    return scaled - 0.5;
  }

  public static double arrayRoundTrip(double first, double second) {
    double[] values = new double[3];
    values[0] = first;
    values[1] = values[0] + second;
    values[2] = values[1] * 2.0;
    return values[2];
  }

  public static float arrayFloat(float value) {
    float[] values = new float[2];
    values[1] = value;
    return values[1];
  }

  public static double arrayDefault() {
    double[] values = new double[1];
    return values[0] + 1.5;
  }

  // Boxing and printing.
  public static double boxedAdd(double value) {
    Double boxed = value;
    return boxed + 0.5;
  }

  public static float boxedFloatAdd(float value) {
    Float boxed = value;
    return boxed + 0.5F;
  }

  public static String printed(double value) {
    return Double.toString(value);
  }

  public static String printedFloat(float value) {
    return Float.toString(value);
  }

  public static String valueOf(double value) {
    return String.valueOf(value);
  }

  public static String built(double value) {
    // A chained expression keeps the builder contents on the marker the next
    // `append` reads; a separate `sb.append(...)` result is discarded by javac.
    return new StringBuilder().append("v=").append(value).toString();
  }

  public static String formatted(double value) {
    return String.format("%f", value);
  }

  public static double parsed(String text) {
    return Double.parseDouble(text);
  }

  public static float parsedFloat(String text) {
    return Float.parseFloat(text);
  }

  public void onInitialize() {
    if (foldedHalfMax() <= 0.0) {
      throw new IllegalStateException("double was truncated to 32 bits");
    }
  }
}
"#;

fn jar(tag: &str) -> NativeJar {
    NativeJar::from_path(&build_fixture(
        tag,
        "fixture-float",
        "demo.FloatEntrypoint",
        SOURCE,
    ))
    .unwrap()
}

/// Call a static fixture method and return its value.
fn call(jar: &NativeJar, name: &str, descriptor: &str, args: &[Value]) -> Value {
    jar.invoke_method_with_args(
        "demo.FloatEntrypoint",
        name,
        descriptor,
        None,
        args,
        &mut StdHost::new(),
    )
    .unwrap_or_else(|error| panic!("{name}{descriptor} failed: {error}"))
}

/// A `Value::Float` bit-for-bit, including NaN.
fn exact(value: f64) -> Value {
    Value::Float(value)
}

#[test]
fn float_entrypoint_executes_without_truncation() {
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
    assert_eq!(report.class, "demo.FloatEntrypoint");
    assert_eq!(report.method, "onInitialize");
}

#[test]
fn double_constants_and_arithmetic_keep_every_bit() {
    let jar = jar("arithmetic");
    // 0.1 + 0.2 is folded to one `ldc2_w`; the exact binary sum is *not* 0.3.
    assert_eq!(call(&jar, "foldedSum", "()D", &[]), exact(0.1 + 0.2));
    assert_eq!(
        call(&jar, "foldedHalfMax", "()D", &[]),
        exact(f64::MAX / 2.0)
    );
    assert_eq!(call(&jar, "maxValue", "()D", &[]), exact(f64::MAX));
    assert_eq!(
        call(&jar, "maxFloat", "()F", &[]),
        exact(f64::from(f32::MAX))
    );

    assert_eq!(
        call(
            &jar,
            "add",
            "(DD)D",
            &[Value::Float(0.1), Value::Float(0.2)]
        ),
        exact(0.1 + 0.2)
    );
    assert_eq!(
        call(
            &jar,
            "subtract",
            "(DD)D",
            &[Value::Float(1.5), Value::Float(0.25)]
        ),
        exact(1.25)
    );
    assert_eq!(
        call(
            &jar,
            "multiply",
            "(DD)D",
            &[Value::Float(1.5), Value::Float(0.25)]
        ),
        exact(0.375)
    );
    assert_eq!(
        call(
            &jar,
            "divide",
            "(DD)D",
            &[exact(f64::MAX), Value::Float(2.0)]
        ),
        exact(f64::MAX / 2.0)
    );
    assert_eq!(
        call(
            &jar,
            "remainder",
            "(DD)D",
            &[Value::Float(9.5), Value::Float(2.0)]
        ),
        exact(1.5)
    );
    assert_eq!(
        call(&jar, "negate", "(D)D", &[Value::Float(2.5)]),
        exact(-2.5)
    );
}

#[test]
fn float_arithmetic_rounds_to_f32() {
    let jar = jar("f32");
    // An `f32` sum rounds to f32 after the operation; the widened `f64` answer
    // for `0.1 + 0.2` is not the same value.
    assert_eq!(
        call(
            &jar,
            "addFloat",
            "(FF)F",
            &[Value::Float(0.1), Value::Float(0.2)]
        ),
        exact(f64::from(0.1f32 + 0.2f32))
    );
    assert!(
        call(
            &jar,
            "addFloat",
            "(FF)F",
            &[Value::Float(0.1), Value::Float(0.2)]
        ) != exact(0.1 + 0.2)
    );
    assert_eq!(
        call(
            &jar,
            "divFloat",
            "(FF)F",
            &[Value::Float(1.0), Value::Float(3.0)]
        ),
        exact(f64::from(1.0f32 / 3.0f32))
    );
    assert_eq!(
        call(&jar, "negateFloat", "(F)F", &[Value::Float(2.5)]),
        exact(-2.5)
    );
    assert_eq!(
        call(
            &jar,
            "addFloat",
            "(FF)F",
            &[Value::Float(0.0), Value::Float(0.0)]
        ),
        exact(0.0)
    );
}

#[test]
fn conversions_narrow_widen_and_saturate_like_a_jvm_cast() {
    let jar = jar("conversions");
    assert_eq!(
        call(&jar, "widenFloat", "(F)D", &[Value::Float(0.1)]),
        exact(f64::from(0.1f32))
    );
    assert_eq!(
        call(&jar, "narrowDouble", "(D)F", &[exact(0.1 + 0.2)]),
        exact(f64::from((0.1 + 0.2) as f32))
    );
    // Doubles beyond the `f32` range narrow to infinity.
    assert_eq!(
        call(&jar, "narrowDouble", "(D)F", &[exact(f64::MAX)]),
        exact(f64::INFINITY)
    );
    assert_eq!(
        call(&jar, "widenLong", "(J)D", &[Value::Long(4_294_967_297)]),
        exact(4_294_967_297_f64)
    );
    assert_eq!(
        call(&jar, "longToFloat", "(J)F", &[Value::Long(4_294_967_297)]),
        exact(f64::from(4_294_967_297i64 as f32))
    );
    assert_eq!(
        call(&jar, "intToFloat", "(I)F", &[Value::Int(16_777_217)]),
        exact(f64::from(16_777_217i32 as f32))
    );
    assert_eq!(
        call(&jar, "intToDouble", "(I)D", &[Value::Int(i32::MAX)]),
        exact(f64::from(i32::MAX))
    );

    // Narrowing to int/long truncates toward zero and saturates, as the JVM
    // specifies; `NaN` becomes zero.
    assert_eq!(
        call(&jar, "truncateDouble", "(D)I", &[Value::Float(-3.75)]),
        Value::Int(-3)
    );
    assert_eq!(
        call(&jar, "truncateDouble", "(D)I", &[exact(f64::NAN)]),
        Value::Int(0)
    );
    assert_eq!(
        call(&jar, "truncateDouble", "(D)I", &[exact(1e300)]),
        Value::Int(i32::MAX)
    );
    assert_eq!(
        call(&jar, "truncateLong", "(D)J", &[exact(-1e30)]),
        Value::Long(i64::MIN)
    );
    assert_eq!(
        call(
            &jar,
            "truncateLong",
            "(D)J",
            &[Value::Float(4_294_967_297.9)]
        ),
        Value::Long(4_294_967_297)
    );
    assert_eq!(
        call(&jar, "truncateFloat", "(F)I", &[Value::Float(7.9)]),
        Value::Int(7)
    );
    assert_eq!(
        call(&jar, "truncateFloatLong", "(F)J", &[Value::Float(-9.9)]),
        Value::Long(-9)
    );
}

#[test]
fn comparisons_and_nan_follow_the_jvm_compare_opcodes() {
    let jar = jar("compare");
    assert_eq!(
        call(
            &jar,
            "compare",
            "(DD)I",
            &[Value::Float(-1.5), Value::Float(2.5)]
        ),
        Value::Int(-1)
    );
    assert_eq!(
        call(
            &jar,
            "compare",
            "(DD)I",
            &[Value::Float(2.5), Value::Float(2.5)]
        ),
        Value::Int(0)
    );
    assert_eq!(
        call(
            &jar,
            "compare",
            "(DD)I",
            &[Value::Float(2.5), Value::Float(-1.5)]
        ),
        Value::Int(1)
    );
    assert_eq!(
        call(
            &jar,
            "compareFloat",
            "(FF)I",
            &[Value::Float(-1.5), Value::Float(2.5)]
        ),
        Value::Int(-1)
    );
    // Any comparison with NaN is false.
    assert_eq!(
        call(&jar, "less", "(DD)Z", &[exact(f64::NAN), Value::Float(1.0)]),
        Value::Int(0)
    );
    assert_eq!(
        call(
            &jar,
            "greater",
            "(DD)Z",
            &[exact(f64::NAN), Value::Float(1.0)]
        ),
        Value::Int(0)
    );
    assert_eq!(
        call(
            &jar,
            "lessFloat",
            "(FF)Z",
            &[exact(f64::from(f32::NAN)), Value::Float(1.0)]
        ),
        Value::Int(0)
    );
    assert_eq!(
        call(
            &jar,
            "greaterFloat",
            "(FF)Z",
            &[Value::Float(1.0), exact(f64::from(f32::NAN))]
        ),
        Value::Int(0)
    );
    assert_eq!(
        call(&jar, "same", "(D)Z", &[exact(f64::NAN)]),
        Value::Int(0)
    );
    assert_eq!(
        call(&jar, "same", "(D)Z", &[Value::Float(1.0)]),
        Value::Int(1)
    );

    // Boxing/`Double.compare` puts NaN above every value.
    assert_eq!(
        call(
            &jar,
            "compareTotal",
            "(DD)I",
            &[exact(f64::NAN), Value::Float(1.0)]
        ),
        Value::Int(1)
    );
    assert_eq!(
        call(&jar, "isNaN", "(D)Z", &[exact(f64::NAN)]),
        Value::Int(1)
    );
    assert_eq!(
        call(&jar, "isNaN", "(D)Z", &[Value::Float(1.0)]),
        Value::Int(0)
    );
    assert_eq!(
        call(&jar, "isInfinite", "(D)Z", &[exact(f64::NEG_INFINITY)]),
        Value::Int(1)
    );
    let nan = call(&jar, "nan", "()D", &[]);
    assert!(
        matches!(nan, Value::Float(value) if value.is_nan()),
        "0.0 / 0.0 is NaN, got {nan:?}"
    );
}

#[test]
fn double_locals_arrays_and_defaults_round_trip() {
    let jar = jar("locals");
    let value = 1_000_000.25;
    assert_eq!(
        call(&jar, "localRoundTrip", "(D)D", &[Value::Float(value)]),
        exact(value / 2.0 * 3.0 - 0.5)
    );
    assert_eq!(
        call(
            &jar,
            "arrayRoundTrip",
            "(DD)D",
            &[exact(0.1 + 0.2), Value::Float(0.5)]
        ),
        exact((0.1 + 0.2 + 0.5) * 2.0)
    );
    assert_eq!(
        call(&jar, "arrayFloat", "(F)F", &[Value::Float(0.1)]),
        exact(f64::from(0.1f32))
    );
    // A `new double[1]` element is +0.0 before the write.
    assert_eq!(call(&jar, "arrayDefault", "()D", &[]), exact(1.5));
}

#[test]
fn boxing_and_printing_use_the_java_spelling() {
    let jar = jar("boxing");
    assert_eq!(
        call(&jar, "boxedAdd", "(D)D", &[Value::Float(1.5)]),
        exact(2.0)
    );
    assert_eq!(
        call(&jar, "boxedFloatAdd", "(F)F", &[Value::Float(1.5)]),
        exact(2.0)
    );
    assert_eq!(
        call(
            &jar,
            "printed",
            "(D)Ljava/lang/String;",
            &[exact(0.1 + 0.2)]
        ),
        Value::Ref(Some("0.30000000000000004".into()))
    );
    // `Float.toString` prints the `f32` digits, not the widened `f64` ones.
    assert_eq!(
        call(
            &jar,
            "printedFloat",
            "(F)Ljava/lang/String;",
            &[Value::Float(0.1)]
        ),
        Value::Ref(Some("0.1".into()))
    );
    assert_eq!(
        call(
            &jar,
            "printedFloat",
            "(F)Ljava/lang/String;",
            &[exact(f64::from(f32::MAX))]
        ),
        Value::Ref(Some("3.4028235E38".into()))
    );
    assert_eq!(
        call(
            &jar,
            "printed",
            "(D)Ljava/lang/String;",
            &[exact(f64::MAX / 2.0)]
        ),
        Value::Ref(Some("8.988465674311579E307".into()))
    );
    assert_eq!(
        call(&jar, "printed", "(D)Ljava/lang/String;", &[exact(f64::NAN)]),
        Value::Ref(Some("NaN".into()))
    );
    assert_eq!(
        call(
            &jar,
            "printed",
            "(D)Ljava/lang/String;",
            &[exact(f64::INFINITY)]
        ),
        Value::Ref(Some("Infinity".into()))
    );
    assert_eq!(
        call(
            &jar,
            "valueOf",
            "(D)Ljava/lang/String;",
            &[Value::Float(2.5)]
        ),
        Value::Ref(Some("2.5".into()))
    );
    assert_eq!(
        call(&jar, "built", "(D)Ljava/lang/String;", &[exact(0.1 + 0.2)]),
        Value::Ref(Some("v=0.30000000000000004".into()))
    );
    assert_eq!(
        call(
            &jar,
            "formatted",
            "(D)Ljava/lang/String;",
            &[Value::Float(1.5)]
        ),
        Value::Ref(Some("1.500000".into()))
    );
    assert_eq!(
        call(
            &jar,
            "parsed",
            "(Ljava/lang/String;)D",
            &[Value::Ref(Some("0.30000000000000004".into()))]
        ),
        exact(0.1 + 0.2)
    );
    assert_eq!(
        call(
            &jar,
            "parsedFloat",
            "(Ljava/lang/String;)F",
            &[Value::Ref(Some("1.25".into()))]
        ),
        exact(1.25)
    );
    assert_eq!(
        call(
            &jar,
            "parsed",
            "(Ljava/lang/String;)D",
            &[Value::Ref(Some("-Infinity".into()))]
        ),
        exact(f64::NEG_INFINITY)
    );
}

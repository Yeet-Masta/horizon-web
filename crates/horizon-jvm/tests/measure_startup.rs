//! Measures a real compiled fixture through `horizon-jvm --measure`: startup
//! time, peak RSS where the OS keeps one, and a bounded child lifecycle.
//!
//! The fixture is ordinary Java (no loader, no façade), so this runs on a
//! machine with a JDK and nothing from `.local/`. A missing `javac` skips it.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};
use zip::write::SimpleFileOptions;

const MOD_SOURCE: &str = r#"
package demo;
public final class Measured {
  public void onInitialize() {
    // Allocate, so the peak RSS is more than an empty JVM's floor.
    Object[] keep = new Object[20000];
    for (int i = 0; i < keep.length; i++) keep[i] = new int[64];
    System.out.println("measure-fixture-ok:" + keep.length);
  }
}
"#;

const SLOW_SOURCE: &str = r#"
package demo;
public final class Slow {
  public void onInitialize() throws Exception {
    // A grandchild, as a mod's own helper process would be, then a sleep the
    // measurement's bound has to end.
    new ProcessBuilder("sleep", "300").start();
    Thread.sleep(120_000);
  }
}
"#;

fn javac() -> PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into())
}

fn javac_available() -> bool {
    Command::new(javac())
        .arg("-version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Compile one class into a Fabric jar with a `main` entrypoint.
fn fixture(root: &Path, name: &str, source: &str, class: &str) -> Option<PathBuf> {
    let file = root.join(format!("demo/{class}.java"));
    let classes = root.join(format!("classes-{class}"));
    fs::create_dir_all(file.parent()?).ok()?;
    fs::create_dir_all(&classes).ok()?;
    fs::write(&file, source).ok()?;
    let status = Command::new(javac())
        .arg("--release")
        .arg("17")
        .arg("-d")
        .arg(&classes)
        .arg(&file)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let jar = root.join(name);
    let mut zip = zip::ZipWriter::new(fs::File::create(&jar).ok()?);
    let options = SimpleFileOptions::default();
    zip.start_file("fabric.mod.json", options).ok()?;
    write!(
        zip,
        r#"{{"schemaVersion":1,"id":"{class}fixture","version":"1.0","entrypoints":{{"main":["demo.{class}"]}}}}"#
    )
    .ok()?;
    zip.start_file(format!("demo/{class}.class"), options)
        .ok()?;
    zip.write_all(&fs::read(classes.join(format!("demo/{class}.class"))).ok()?)
        .ok()?;
    zip.finish().ok()?;
    Some(jar)
}

fn output(command: &mut Command) -> (bool, String) {
    let result = command.output().expect("the probe should start");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    (result.status.success(), text)
}

/// `run0 startup_ms=… total_ms=… cpu_ms=… peak_rss_kib=… status=ok`
fn figure<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.split_whitespace()
        .find_map(|field| field.strip_prefix(&format!("{key}=")))
}

fn number(text: &str, key: &str) -> Option<f64> {
    figure(text, key).and_then(|value| value.parse().ok())
}

#[test]
fn a_fixture_is_measured_with_startup_time_and_peak_rss() {
    if !javac_available() {
        eprintln!("skipping: javac is not available");
        return;
    }
    let root = std::env::temp_dir().join(format!(
        "horizon-jvm-measure-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    let Some(jar) = fixture(&root, "measured.jar", MOD_SOURCE, "Measured") else {
        eprintln!("skipping: this JDK cannot compile the fixture");
        let _ = fs::remove_dir_all(&root);
        return;
    };

    let (ok, text) = output(
        Command::new(env!("CARGO_BIN_EXE_horizon-jvm"))
            .arg("--measure")
            .arg("--measure-repeat")
            .arg("2")
            .arg(&jar),
    );
    assert!(ok, "measurement failed:\n{text}");
    assert!(text.contains("measure: entry demo.Measured"), "{text}");
    assert!(text.contains("2 run(s)"), "{text}");
    // Both samples ran: a repeat that silently measured once would still pass
    // a "contains run0" check.
    assert!(text.contains("run0 ") && text.contains("run1 "), "{text}");

    // Startup time is real: the fixture printed `jvm up:` and it took time.
    let startup = number(&text, "median_startup_ms").expect("startup time");
    let total = number(&text, "median_total_ms").expect("total time");
    assert!(startup > 0.0 && startup <= total, "{text}");
    // The child is a real JVM doing real work, not a stub exiting at once.
    assert!(
        total > 50.0,
        "the measured total is implausibly small:\n{text}"
    );

    // Peak RSS is taken where the OS keeps one (Linux here); where it does not,
    // the report must say n/a rather than print a zero.
    let peak = figure(&text, "median_peak_rss_kib").expect("peak figure");
    if cfg!(target_os = "linux") {
        let kib: f64 = peak.parse().expect("a peak in KiB");
        // A JVM's floor is tens of MB; the fixture's own allocation keeps it
        // above an empty process.
        assert!(
            kib > 8.0 * 1024.0,
            "peak RSS {kib} KiB is too small:\n{text}"
        );
    } else {
        assert_eq!(peak, "n/a", "{text}");
    }

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_fixture_past_the_bound_is_killed_and_fails_the_measurement() {
    if !javac_available() {
        eprintln!("skipping: javac is not available");
        return;
    }
    let root = std::env::temp_dir().join(format!(
        "horizon-jvm-measure-slow-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    let Some(jar) = fixture(&root, "slow.jar", SLOW_SOURCE, "Slow") else {
        eprintln!("skipping: this JDK cannot compile the fixture");
        let _ = fs::remove_dir_all(&root);
        return;
    };

    let started = Instant::now();
    let (ok, text) = output(
        Command::new(env!("CARGO_BIN_EXE_horizon-jvm"))
            .arg("--measure")
            .arg("--measure-timeout")
            .arg("2")
            .arg("--json")
            .arg(&jar),
    );
    let elapsed = started.elapsed();
    // A run past its bound is a failure with a reason, not a hang and not a
    // silent zero.
    assert!(!ok, "a killed fixture must fail the measurement:\n{text}");
    assert!(text.contains("measurement bound"), "{text}");
    assert!(text.contains("\"timedOut\":true"), "{text}");
    // The bound is the point: it returned near its deadline, not at the
    // fixture's own 120 s sleep.
    assert!(elapsed.as_secs() < 30, "bound took {elapsed:?}:\n{text}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn tuning_flags_without_measure_are_refused() {
    // Measuring nothing while looking like a measurement is the failure mode
    // worth a test of its own.
    let (ok, text) = output(
        Command::new(env!("CARGO_BIN_EXE_horizon-jvm"))
            .arg("--measure-repeat")
            .arg("3")
            .arg("absent.jar"),
    );
    assert!(!ok, "{text}");
    assert!(text.contains("need --measure"), "{text}");
}

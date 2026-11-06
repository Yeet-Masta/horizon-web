//! Runs a Java program's `main` on the interpreter, for the conformance
//! harness (`tools/java_conformance.py`).
//!
//! ```text
//! horizon-java-run [--engine vm|bounded] [--max-calls N] [-ea] [-Dk=v] <jar> <main-class> [args...]
//! ```
//!
//! `--engine vm` (the default) runs the program on the VM (`src/vm`);
//! `--engine bounded` on the bounded interpreter the game used before it.
//!
//! It behaves like the `java` launcher as far as the harness compares: the
//! program's output goes to stdout, an uncaught exception prints
//! `Exception in thread "main" <class>: <message>` on stderr and exits 1, and
//! `System.exit(n)` exits `n`. When the interpreter itself stops (an opcode or
//! library call it does not model) the runner prints `horizon-java-run:
//! interpreter error: ...` and exits 70, so the harness can tell an
//! unsupported program from a wrong answer.
use horizon_native_java::{
    Host, MethodRef, MixinSide, NativeError, NativeJar, RegistryDeclaration, StdHost, Value,
};
use horizon_native_java::vm;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::process::ExitCode;

/// What a run ended with, before it becomes an exit code.
enum Outcome {
    Normal,
    Exit(i32),
    Uncaught(String, Option<String>),
    Interpreter(String),
}

const STDOUT_MARKER: &str = "horizon-java-run/stdout";
const STDERR_MARKER: &str = "horizon-java-run/stderr";
/// The error a `System.exit` call unwinds the interpreter with.
const EXIT_PREFIX: &str = "horizon-java-run/exit/";

/// The bounded interpreter's standard host, with `System.out`/`System.err`
/// printing to the process's streams and `System.exit` ending the run.
struct ConsoleHost {
    inner: StdHost,
    out: Vec<u8>,
    err: Vec<u8>,
}

impl ConsoleHost {
    fn stream(&mut self, receiver: &Value) -> Option<&mut Vec<u8>> {
        match receiver {
            Value::Ref(Some(marker)) if marker == STDOUT_MARKER => Some(&mut self.out),
            Value::Ref(Some(marker)) if marker == STDERR_MARKER => Some(&mut self.err),
            _ => None,
        }
    }

    fn print(&mut self, r: &MethodRef, receiver: &Value, args: &[Value]) -> Option<Value> {
        if r.owner != "java/io/PrintStream" {
            return None;
        }
        let newline = match r.name.as_str() {
            "println" => true,
            "print" => false,
            _ => return None,
        };
        let param = r
            .descriptor
            .strip_prefix('(')
            .and_then(|rest| rest.split(')').next())
            .unwrap_or("");
        let text = match (param, args.first()) {
            ("", _) => String::new(),
            (_, None) => return None,
            (desc, Some(value)) => format_value(desc, value),
        };
        let stream = self.stream(receiver)?;
        stream.extend_from_slice(text.as_bytes());
        if newline {
            stream.push(b'\n');
        }
        Some(Value::Void)
    }
}

/// A printed argument as `PrintStream.print` writes it, from its parameter
/// descriptor: the bounded model keeps one numeric variant per width class.
fn format_value(desc: &str, value: &Value) -> String {
    match (desc, value) {
        ("Z", Value::Int(v)) => (if *v != 0 { "true" } else { "false" }).to_owned(),
        ("C", Value::Int(v)) => char::from_u32(*v as u32 & 0xffff)
            .map(String::from)
            .unwrap_or_else(|| "\u{fffd}".to_owned()),
        ("F", Value::Float(v)) => java_float_to_string(*v as f32),
        ("D", Value::Float(v)) => java_double_to_string(*v),
        (_, Value::Int(v)) => v.to_string(),
        (_, Value::Long(v)) => v.to_string(),
        (_, Value::Float(v)) => java_double_to_string(*v),
        (_, Value::Ref(Some(text))) => text.clone(),
        (_, Value::Ref(None)) => "null".to_owned(),
        (_, Value::Void) => String::new(),
    }
}

/// `Double.toString`: the shortest decimal that rounds to the value (at least
/// two significant digits, JDK 19+), laid out plainly for magnitudes in
/// [10^-3, 10^7) and in computerized scientific notation otherwise.
fn java_double_to_string(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    let shortest = format!("{:e}", v.abs());
    let two = format!("{:.1e}", v.abs());
    layout(
        v.is_sign_negative(),
        &shortest,
        &two,
        v.abs() >= 1e-3 && v.abs() < 1e7,
    )
}

/// `Float.toString`, the same rules over `float`'s precision.
fn java_float_to_string(v: f32) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    let shortest = format!("{:e}", v.abs());
    let two = format!("{:.1e}", v.abs());
    layout(
        v.is_sign_negative(),
        &shortest,
        &two,
        v.abs() >= 1e-3 && v.abs() < 1e7,
    )
}

/// Lays out Rust's shortest `d.ddde±n` digits the way Java prints them.
fn layout(negative: bool, shortest: &str, two_digits: &str, plain: bool) -> String {
    let (mantissa, exponent) = shortest.split_once('e').unwrap_or((shortest, "0"));
    let mut digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let mut exponent: i32 = exponent.parse().unwrap_or(0);
    if digits.len() == 1 {
        // One significant digit: Java prints the closest two-digit decimal.
        let (mantissa, e) = two_digits.split_once('e').unwrap_or((two_digits, "0"));
        digits = mantissa.chars().filter(char::is_ascii_digit).collect();
        exponent = e.parse().unwrap_or(0);
    }
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if plain {
        if exponent >= 0 {
            let point = exponent as usize + 1;
            if digits.len() <= point {
                out.push_str(digits);
                out.extend(std::iter::repeat_n('0', point - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..point]);
                out.push('.');
                out.push_str(&digits[point..]);
            }
        } else {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', (-exponent - 1) as usize));
            out.push_str(digits);
        }
    } else {
        out.push_str(&digits[..1]);
        out.push('.');
        if digits.len() > 1 {
            out.push_str(&digits[1..]);
        } else {
            out.push('0');
        }
        out.push('E');
        out.push_str(&exponent.to_string());
    }
    out
}

impl Host for ConsoleHost {
    fn get_static(&mut self, r: &MethodRef) -> Result<Value, NativeError> {
        if r.owner == "java/lang/System" && r.descriptor == "Ljava/io/PrintStream;" {
            match r.name.as_str() {
                "out" => return Ok(Value::Ref(Some(STDOUT_MARKER.into()))),
                "err" => return Ok(Value::Ref(Some(STDERR_MARKER.into()))),
                _ => {}
            }
        }
        self.inner.get_static(r)
    }

    fn invoke(&mut self, r: &MethodRef, args: &[Value]) -> Result<Value, NativeError> {
        if r.owner == "java/lang/System" && r.name == "exit" && r.descriptor == "(I)V" {
            let code = match args.first() {
                Some(Value::Int(code)) => *code,
                _ => 0,
            };
            return Err(NativeError::Metadata(format!("{EXIT_PREFIX}{code}")));
        }
        self.inner.invoke(r, args)
    }

    fn invoke_instance(
        &mut self,
        r: &MethodRef,
        receiver: &Value,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        if let Some(value) = self.print(r, receiver, args) {
            return Ok(value);
        }
        self.inner.invoke_instance(r, receiver, args)
    }

    fn mutable_collection_values(&self, value: &Value) -> Option<Vec<String>> {
        self.inner.mutable_collection_values(value)
    }

    fn service_providers(&self, service: &str) -> Vec<String> {
        Host::service_providers(&self.inner, service)
    }

    fn throwable_message(&self, marker: &str) -> Option<String> {
        self.inner.throwable_message(marker)
    }

    fn record_throwable(&mut self, marker: &str, message: Option<String>) {
        self.inner.record_throwable(marker, message);
    }
}

fn run(jar: &Path, main_class: &str) -> (Outcome, Vec<u8>, Vec<u8>) {
    let jar = match NativeJar::from_path(jar) {
        Ok(jar) => jar,
        Err(error) => return (Outcome::Interpreter(error.to_string()), vec![], vec![]),
    };
    let mut host = ConsoleHost {
        inner: jar.seed_host(StdHost::new()),
        out: Vec::new(),
        err: Vec::new(),
    };
    let result = jar.invoke_method_with_args(
        &main_class.replace('.', "/"),
        "main",
        "([Ljava/lang/String;)V",
        None,
        &[Value::Ref(None)],
        &mut host,
    );
    let outcome = match result {
        Ok(_) => Outcome::Normal,
        Err(NativeError::Metadata(text)) if text.starts_with(EXIT_PREFIX) => {
            Outcome::Exit(text[EXIT_PREFIX.len()..].parse().unwrap_or(1))
        }
        Err(NativeError::UncaughtException { class, message, .. }) => {
            Outcome::Uncaught(class.replace('/', "."), message)
        }
        Err(error) => Outcome::Interpreter(error.to_string()),
    };
    (outcome, host.out, host.err)
}

/// A declaration as the harness compares it: its kind and string properties.
fn declaration_json(declaration: &RegistryDeclaration) -> serde_json::Value {
    use serde_json::json;
    match declaration {
        RegistryDeclaration::Block {
            template,
            properties,
            item,
        } => json!({"kind": "block", "template": template, "properties": properties, "item": item}),
        RegistryDeclaration::Item {
            template,
            properties,
        } => json!({"kind": "item", "template": template, "properties": properties}),
        RegistryDeclaration::MobEffect { category, color } => {
            json!({"kind": "mob_effect", "category": category, "color": color})
        }
        RegistryDeclaration::Recipe { json } => json!({"kind": "recipe", "json": json}),
        RegistryDeclaration::Ingredient { json } => json!({"kind": "ingredient", "json": json}),
        RegistryDeclaration::Custom { kind, properties } => {
            json!({"kind": "custom", "custom": kind, "properties": properties})
        }
    }
}

/// `--mod <client|server> <jar>`: runs the jar's entrypoints for one role the
/// way the game's loader does (`game/java_mods.rs`, `run_one`): the loader
/// metadata picks the entrypoint kind, the load-time Mixin weave is applied,
/// and the host is seeded with the environment and the mod ids. Prints one
/// JSON object: how many entrypoints there were and how many completed, the
/// first error, and every registration with its typed declaration.
fn run_mod(role: &str, path: &Path) -> serde_json::Value {
    use serde_json::json;
    let client = role == "client";
    let jar = match NativeJar::from_path(path) {
        Ok(jar) => jar,
        Err(error) => {
            return json!({"status": "error", "stage": "load", "error": error.to_string()});
        }
    };
    if jar.client_side_only && !client {
        return json!({"status": "skipped", "stage": "metadata", "error": "clientSideOnly"});
    }
    let kinds = if client {
        ["client", "main", "neoforge"]
    } else {
        ["server", "main", "neoforge"]
    };
    let Some((kind, entries)) = kinds.iter().find_map(|kind| {
        let entries = jar.discover_entrypoints(kind);
        (!entries.is_empty()).then_some((*kind, entries))
    }) else {
        return json!({"status": "skipped", "stage": "discover", "error": "no entrypoint"});
    };
    if let Some(adapter) = entries.iter().find_map(|entry| entry.adapter.clone()) {
        return json!({
            "status": "error", "stage": "discover", "kind": kind, "entrypoints": entries.len(),
            "completed": 0, "error": format!("entrypoint adapter {adapter}"),
        });
    }
    let weave = jar.plan_load_time_weave(if client {
        MixinSide::Client
    } else {
        MixinSide::Server
    });
    let jar = match jar.with_woven_classes(&weave.classes) {
        Ok(jar) => jar,
        Err(error) => {
            return json!({"status": "error", "stage": "weave", "error": error.to_string()});
        }
    };
    let mut host = StdHost::new().with_environment_type(if client { "CLIENT" } else { "SERVER" });
    let id = jar
        .native_id
        .clone()
        .or_else(|| jar.fabric_id.clone())
        .or_else(|| jar.neoforge_id.clone())
        .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()));
    if let Some(id) = id {
        let version = jar
            .fabric_version
            .clone()
            .or_else(|| jar.neoforge_version.clone())
            .unwrap_or_else(|| "unknown".into());
        host = host.with_mod(id, version);
    }
    for dependency in &jar.required_mods {
        host = host.with_mod(dependency, "unknown");
    }
    let mut host = jar.seed_host(host);
    let mut completed = 0;
    let mut calls = 0;
    let mut error = None;
    for entry in &entries {
        match jar.execute(&entry.class, entry.method.as_deref(), kind, &mut host) {
            Ok(report) => {
                completed += 1;
                calls += report.calls;
            }
            Err(failure) => {
                error = Some(format!("{}: {failure}", entry.class));
                break;
            }
        }
    }
    let registrations = host
        .registrations()
        .iter()
        .map(|entry| {
            json!({
                "registry": entry.registry,
                "id": entry.id,
                "declaration": entry.declaration.as_ref().map(declaration_json),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "status": if error.is_none() { "ok" } else { "error" },
        "stage": "execute",
        "kind": kind,
        "entrypoints": entries.len(),
        "completed": completed,
        "error": error,
        "host_calls": calls,
        "woven": weave.woven.len(),
        "weave_skipped": weave.skipped.len(),
        "registrations": registrations,
        "payloads": host.payload_registrations().iter().map(|p| p.direction.clone()).collect::<Vec<_>>(),
    })
}

/// Runs `main` on the VM, as the `java` launcher would.
fn run_vm(jar: &Path, main_class: &str, args: &[String], options: vm::Options) -> ExitCode {
    let bytes = match std::fs::read(jar) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("horizon-java-run: {}: {e}", jar.display());
            return ExitCode::from(2);
        }
    };
    let mut path = vm::JarClassPath::new();
    if let Err(e) = path.add_jar(&bytes) {
        eprintln!("horizon-java-run: {}: {e}", jar.display());
        return ExitCode::from(2);
    }
    let run = vm::run_main(Arc::new(path), main_class, args, options);
    let _ = std::io::stdout().write_all(&run.stdout);
    let _ = std::io::stderr().write_all(&run.stderr);
    match run.outcome {
        vm::MainOutcome::Returned => ExitCode::SUCCESS,
        vm::MainOutcome::Exited(code) => ExitCode::from(code as u8),
        vm::MainOutcome::Uncaught { .. } => ExitCode::from(1),
        vm::MainOutcome::Stopped(why) => {
            eprintln!("horizon-java-run: interpreter error: {why}");
            ExitCode::from(70)
        }
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let mut engine = "vm".to_string();
    if let Some(at) = args.iter().position(|a| a == "--engine") {
        if let Some(e) = args.get(at + 1) {
            engine = e.clone();
        }
        args.drain(at..(at + 2).min(args.len()));
    }
    if let Some(at) = args.iter().position(|a| a == "--max-calls") {
        // Accepted for the harness's command line; the bounded engine has its
        // own fixed limits.
        args.drain(at..(at + 2).min(args.len()));
    }
    if args.first().map(String::as_str) == Some("--mod") {
        let (Some(role), Some(jar)) = (args.get(1), args.get(2)) else {
            eprintln!("usage: horizon-java-run --mod <client|server> <jar>");
            return ExitCode::from(2);
        };
        let report = run_mod(role, Path::new(jar));
        println!("{report}");
        return ExitCode::SUCCESS;
    }
    // Launcher options the harness passes to HotSpot too: assertions and
    // system properties. The bounded engine has no use for them.
    let mut options = vm::Options::default();
    while args
        .first()
        .is_some_and(|arg| arg.starts_with("-D") || arg.starts_with("-ea") || arg == "-da")
    {
        let arg = args.remove(0);
        if let Some(prop) = arg.strip_prefix("-D") {
            let (k, v) = prop.split_once('=').unwrap_or((prop, ""));
            options.properties.push((k.to_string(), v.to_string()));
        } else if arg.starts_with("-ea") {
            options.assertions = true;
        }
    }
    let (Some(jar), Some(main_class)) = (args.first(), args.get(1)) else {
        eprintln!("usage: horizon-java-run <jar> <main-class> [args...]");
        return ExitCode::from(2);
    };
    if engine == "vm" {
        return run_vm(Path::new(jar), main_class, &args[2..], options);
    }
    let (outcome, out, err) = run(Path::new(jar), main_class);
    let _ = std::io::stdout().write_all(&out);
    let _ = std::io::stderr().write_all(&err);
    match outcome {
        Outcome::Normal => ExitCode::SUCCESS,
        Outcome::Exit(code) => ExitCode::from(code as u8),
        Outcome::Uncaught(class, message) => {
            match message {
                Some(message) => eprintln!("Exception in thread \"main\" {class}: {message}"),
                None => eprintln!("Exception in thread \"main\" {class}"),
            }
            ExitCode::from(1)
        }
        Outcome::Interpreter(error) => {
            eprintln!("horizon-java-run: interpreter error: {error}");
            ExitCode::from(70)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{java_double_to_string, java_float_to_string};

    #[test]
    fn doubles_print_as_java_does() {
        // Expected strings are what HotSpot 25 prints for the same values.
        for (value, java) in [
            (1.0, "1.0"),
            (0.1, "0.1"),
            (100.0, "100.0"),
            (1e7, "1.0E7"),
            (9999999.0, "9999999.0"),
            (0.001, "0.001"),
            (0.0001, "1.0E-4"),
            (1e23, "1.0E23"),
            (f64::MIN_POSITIVE * f64::EPSILON, "4.9E-324"),
            (f64::MAX, "1.7976931348623157E308"),
            (-2.5, "-2.5"),
            (1.0 / 3.0, "0.3333333333333333"),
            (123456.789, "123456.789"),
        ] {
            assert_eq!(java_double_to_string(value), java, "{value:e}");
        }
    }

    #[test]
    fn floats_print_as_java_does() {
        for (value, java) in [
            (1.0f32, "1.0"),
            (0.1f32, "0.1"),
            (1.0f32 / 3.0, "0.33333334"),
            (1e10f32, "1.0E10"),
            (f32::MIN_POSITIVE, "1.1754944E-38"),
            (3.4028235e38f32, "3.4028235E38"),
        ] {
            assert_eq!(java_float_to_string(value), java, "{value:e}");
        }
    }
}

//! `CONSTANT_Dynamic` (condy) preservation in the bounded class writer.
//!
//! `javac` never emits condy for the fixture below (string concatenation uses
//! `invokedynamic`), so the test splices a *valid* condy entry into the compiled
//! class: a `ConstantBootstraps.nullConstant` bootstrap, its `BootstrapMethods`
//! row, and a static `condy()` method that executes the constant.  Weaving a
//! `HEAD` and a `RETURN` plan appends constants to the pool; every original
//! index, including the condy's name/type and bootstrap row, must survive.
//!
//! The rewritten class is loaded in a fresh JVM and compared with the unwoven
//! original, and a plain class without condy still round-trips byte-for-byte.

use horizon_native_java::{ClassFile, MethodRef, MixinInjectionPlan, weave_class};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const TARGET: &str = r#"package demo;
public class Target {
    public static int calls;
    public static String sink = "";
    public String greet(String name) { return "hi " + name; }
    public void note(String value) { sink = "n:" + value; }
}"#;

const CALLBACK: &str = r#"package demo;
public final class Callback {
    public static void onGreet(Target self, String name) { Target.calls++; }
    public static void onNote(String value) { Target.calls++; }
}"#;

const RUNNER: &str = r#"import java.lang.reflect.Method;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Path;
import java.nio.file.Paths;

class Runner {
    public static void main(String[] args) throws Exception {
        Path dir = Paths.get(args[0]);
        try (URLClassLoader loader =
                new URLClassLoader(new URL[] { dir.toUri().toURL() }, Runner.class.getClassLoader())) {
            Class<?> target = Class.forName("demo.Target", true, loader);
            Object instance = target.getConstructor().newInstance();
            Object greeting = target.getMethod("greet", String.class).invoke(instance, "x");
            target.getMethod("note", String.class).invoke(instance, "y");
            System.out.println("greet=" + greeting);
            System.out.println("sink=" + target.getField("sink").get(null));
            System.out.println("calls=" + target.getField("calls").getInt(null));
            String condy;
            try {
                Object value = target.getMethod("condy").invoke(null);
                condy = "condy=" + value;
            } catch (NoSuchMethodException absent) {
                condy = "condy=absent";
            }
            System.out.println(condy);
        }
    }
}
"#;

fn javac() -> PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into())
}

fn java() -> Option<PathBuf> {
    let path = std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/java"))
        .unwrap_or_else(|| "java".into());
    Command::new(&path).arg("-version").output().ok()?;
    Some(path)
}

fn compile(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-weaver-condy-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src/demo");
    let classes = root.join("classes");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&classes).unwrap();
    fs::write(src.join("Target.java"), TARGET).unwrap();
    fs::write(src.join("Callback.java"), CALLBACK).unwrap();
    let status = Command::new(javac())
        .arg("--release")
        .arg("17")
        .arg("-d")
        .arg(&classes)
        .arg(src.join("Target.java"))
        .arg(src.join("Callback.java"))
        .status()
        .unwrap();
    assert!(status.success(), "javac failed for {name}");
    classes
}

fn read_class(classes: &Path, name: &str) -> Vec<u8> {
    fs::read(classes.join(format!("{name}.class"))).unwrap()
}

fn method(owner: &str, name: &str, descriptor: &str) -> MethodRef {
    MethodRef {
        owner: owner.into(),
        name: name.into(),
        descriptor: descriptor.into(),
    }
}

// ---------------------------------------------------------------------------
// A minimal class-file splitter/builder, enough to append one valid condy.
// ---------------------------------------------------------------------------

struct ClassParts {
    version: Vec<u8>,
    /// `(tag, bytes after the tag)`, in slot order.
    entries: Vec<(u8, Vec<u8>)>,
    slot_count: u16,
    after_pool: Vec<u8>,
    /// Field and method tables, count included.
    fields: Vec<u8>,
    methods: Vec<u8>,
    method_count: u16,
    attributes: Vec<(u16, Vec<u8>)>,
}

fn u2(bytes: &[u8], at: &mut usize) -> u16 {
    let value = u16::from_be_bytes([bytes[*at], bytes[*at + 1]]);
    *at += 2;
    value
}

fn u4(bytes: &[u8], at: &mut usize) -> u32 {
    let value = u32::from_be_bytes([bytes[*at], bytes[*at + 1], bytes[*at + 2], bytes[*at + 3]]);
    *at += 4;
    value
}

fn push_u2(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn push_u4(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn utf8_payload(value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    push_u2(&mut out, value.len() as u16);
    out.extend_from_slice(value.as_bytes());
    out
}

fn pair_payload(first: u16, second: u16) -> Vec<u8> {
    let mut out = Vec::new();
    push_u2(&mut out, first);
    push_u2(&mut out, second);
    out
}

fn skip_attributes(bytes: &[u8], at: &mut usize) {
    let count = u2(bytes, at) as usize;
    for _ in 0..count {
        let _name = u2(bytes, at);
        let length = u4(bytes, at) as usize;
        *at += length;
    }
}

fn split(bytes: &[u8]) -> ClassParts {
    assert_eq!(&bytes[..4], &0xCAFE_BABE_u32.to_be_bytes(), "bad magic");
    let version = bytes[..8].to_vec();
    let mut at = 8;
    let slot_count = u2(bytes, &mut at);
    let mut entries = Vec::new();
    let mut index = 1usize;
    while index < slot_count as usize {
        let tag = bytes[at];
        at += 1;
        let start = at;
        match tag {
            1 => {
                let length = u2(bytes, &mut at) as usize;
                at += length;
            }
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => at += 4,
            5 | 6 => {
                at += 8;
                index += 1;
            }
            7 | 8 | 16 | 19 | 20 => at += 2,
            15 => at += 3,
            other => panic!("unexpected constant pool tag {other}"),
        }
        entries.push((tag, bytes[start..at].to_vec()));
        index += 1;
    }
    let after_start = at;
    at += 6; // access_flags, this_class, super_class
    let interfaces = u2(bytes, &mut at) as usize;
    at += interfaces * 2;
    let after_pool = bytes[after_start..at].to_vec();

    let field_start = at;
    let field_count = u2(bytes, &mut at) as usize;
    for _ in 0..field_count {
        at += 6;
        skip_attributes(bytes, &mut at);
    }
    let fields = bytes[field_start..at].to_vec();

    let method_start = at;
    let method_count = u2(bytes, &mut at);
    for _ in 0..method_count {
        at += 6;
        skip_attributes(bytes, &mut at);
    }
    let methods = bytes[method_start..at].to_vec();

    let attribute_count = u2(bytes, &mut at) as usize;
    let mut attributes = Vec::with_capacity(attribute_count);
    for _ in 0..attribute_count {
        let name = u2(bytes, &mut at);
        let length = u4(bytes, &mut at) as usize;
        attributes.push((name, bytes[at..at + length].to_vec()));
        at += length;
    }
    assert_eq!(at, bytes.len(), "trailing class bytes");
    ClassParts {
        version,
        entries,
        slot_count,
        after_pool,
        fields,
        methods,
        method_count,
        attributes,
    }
}

/// Every `Utf8` entry as `(pool index, value)`.
fn utf8_entries(parts: &ClassParts) -> Vec<(u16, String)> {
    let mut out = Vec::new();
    let mut index = 1usize;
    for (tag, payload) in &parts.entries {
        if *tag == 1 {
            let length = u16::from_be_bytes([payload[0], payload[1]]) as usize;
            out.push((
                index as u16,
                String::from_utf8(payload[2..2 + length].to_vec()).unwrap(),
            ));
        }
        index += if matches!(tag, 5 | 6) { 2 } else { 1 };
    }
    out
}

fn push_entry(parts: &mut ClassParts, next: &mut u16, tag: u8, payload: Vec<u8>) -> u16 {
    let index = *next;
    *next += 1;
    parts.entries.push((tag, payload));
    index
}

fn decode_bootstrap(body: &[u8]) -> Vec<(u16, Vec<u16>)> {
    let mut at = 0;
    let count = u2(body, &mut at) as usize;
    let mut rows = Vec::with_capacity(count);
    for _ in 0..count {
        let handle = u2(body, &mut at);
        let arguments = u2(body, &mut at) as usize;
        let mut values = Vec::with_capacity(arguments);
        for _ in 0..arguments {
            values.push(u2(body, &mut at));
        }
        rows.push((handle, values));
    }
    rows
}

fn encode_bootstrap(rows: &[(u16, Vec<u16>)]) -> Vec<u8> {
    let mut out = Vec::new();
    push_u2(&mut out, rows.len() as u16);
    for (handle, arguments) in rows {
        push_u2(&mut out, *handle);
        push_u2(&mut out, arguments.len() as u16);
        for argument in arguments {
            push_u2(&mut out, *argument);
        }
    }
    out
}

/// Splice one valid `CONSTANT_Dynamic` and the static `condy()` method that
/// executes it into a compiled class.
fn with_condy(bytes: &[u8]) -> Vec<u8> {
    let mut parts = split(bytes);
    let mut next = parts.slot_count;

    let bootstraps_utf8 = push_entry(
        &mut parts,
        &mut next,
        1,
        utf8_payload("java/lang/invoke/ConstantBootstraps"),
    );
    let bootstraps_class = push_entry(&mut parts, &mut next, 7, {
        let mut payload = Vec::new();
        push_u2(&mut payload, bootstraps_utf8);
        payload
    });
    let null_name = push_entry(&mut parts, &mut next, 1, utf8_payload("nullConstant"));
    let null_descriptor = push_entry(
        &mut parts,
        &mut next,
        1,
        utf8_payload(
            "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;",
        ),
    );
    let null_name_type = push_entry(
        &mut parts,
        &mut next,
        12,
        pair_payload(null_name, null_descriptor),
    );
    let null_method = push_entry(
        &mut parts,
        &mut next,
        10,
        pair_payload(bootstraps_class, null_name_type),
    );
    let bootstrap_handle = push_entry(&mut parts, &mut next, 15, {
        let mut payload = vec![6]; // invokeStatic
        push_u2(&mut payload, null_method);
        payload
    });

    let condy_name = push_entry(&mut parts, &mut next, 1, utf8_payload("condyValue"));
    let condy_descriptor = push_entry(&mut parts, &mut next, 1, utf8_payload("Ljava/lang/Object;"));
    let condy_name_type = push_entry(
        &mut parts,
        &mut next,
        12,
        pair_payload(condy_name, condy_descriptor),
    );

    let bootstrap_name = utf8_entries(&parts)
        .into_iter()
        .find(|(_, value)| value == "BootstrapMethods")
        .map(|(index, _)| index);
    let mut rows = parts
        .attributes
        .iter()
        .find(|(name, _)| Some(*name) == bootstrap_name)
        .map(|(_, body)| decode_bootstrap(body))
        .unwrap_or_default();
    let bootstrap_index = rows.len() as u16;
    rows.push((bootstrap_handle, Vec::new()));
    let bootstrap_body = encode_bootstrap(&rows);
    let bootstrap_attribute_name = match bootstrap_name {
        Some(index) => index,
        None => push_entry(&mut parts, &mut next, 1, utf8_payload("BootstrapMethods")),
    };

    let condy = push_entry(
        &mut parts,
        &mut next,
        17,
        pair_payload(bootstrap_index, condy_name_type),
    );

    let method_name = push_entry(&mut parts, &mut next, 1, utf8_payload("condy"));
    let method_descriptor = push_entry(
        &mut parts,
        &mut next,
        1,
        utf8_payload("()Ljava/lang/Object;"),
    );
    let code_name = push_entry(&mut parts, &mut next, 1, utf8_payload("Code"));

    let mut attributes = parts.attributes;
    match attributes
        .iter_mut()
        .find(|(name, _)| *name == bootstrap_attribute_name)
    {
        Some((_, body)) => *body = bootstrap_body,
        None => attributes.push((bootstrap_attribute_name, bootstrap_body)),
    }

    // `public static Object condy() { return <condy>; }`:
    // `ldc_w <condy>; areturn`.
    let mut method = Vec::new();
    push_u2(&mut method, 0x0009); // public static
    push_u2(&mut method, method_name);
    push_u2(&mut method, method_descriptor);
    push_u2(&mut method, 1); // attributes
    push_u2(&mut method, code_name);
    let mut code = Vec::new();
    push_u2(&mut code, 1); // max_stack
    push_u2(&mut code, 0); // max_locals
    push_u4(&mut code, 4); // code_length
    code.push(0x13); // ldc_w
    push_u2(&mut code, condy);
    code.push(0xb0); // areturn
    push_u2(&mut code, 0); // exception table
    push_u2(&mut code, 0); // Code attributes
    push_u4(&mut method, code.len() as u32);
    method.extend_from_slice(&code);

    let mut out = Vec::new();
    out.extend_from_slice(&parts.version);
    push_u2(&mut out, next);
    for (tag, payload) in &parts.entries {
        out.push(*tag);
        out.extend_from_slice(payload);
    }
    out.extend_from_slice(&parts.after_pool);
    out.extend_from_slice(&parts.fields);
    push_u2(&mut out, parts.method_count + 1);
    out.extend_from_slice(&parts.methods[2..]);
    out.extend_from_slice(&method);
    push_u2(&mut out, attributes.len() as u16);
    for (name, body) in attributes {
        push_u2(&mut out, name);
        push_u4(&mut out, body.len() as u32);
        out.extend_from_slice(&body);
    }
    out
}

fn condy_entries(bytes: &[u8]) -> Vec<(u16, u16, u16)> {
    let parts = split(bytes);
    let mut out = Vec::new();
    let mut index = 1usize;
    for (tag, payload) in &parts.entries {
        if *tag == 17 {
            out.push((
                index as u16,
                u16::from_be_bytes([payload[0], payload[1]]),
                u16::from_be_bytes([payload[2], payload[3]]),
            ));
        }
        index += if matches!(tag, 5 | 6) { 2 } else { 1 };
    }
    out
}

fn bootstrap_rows(bytes: &[u8]) -> Vec<(u16, Vec<u16>)> {
    let parts = split(bytes);
    let name = utf8_entries(&parts)
        .into_iter()
        .find(|(_, value)| value == "BootstrapMethods")
        .map(|(index, _)| index);
    parts
        .attributes
        .iter()
        .find(|(index, _)| Some(*index) == name)
        .map(|(_, body)| decode_bootstrap(body))
        .unwrap_or_default()
}

fn run(classes: &Path, target: &[u8], label: &str) -> Vec<String> {
    let Some(java) = java() else {
        return Vec::new();
    };
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-weaver-condy-run-{label}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("demo")).unwrap();
    for entry in fs::read_dir(classes.join("demo")).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "class")
        {
            fs::copy(&path, root.join("demo").join(path.file_name().unwrap())).unwrap();
        }
    }
    fs::write(root.join("demo/Target.class"), target).unwrap();
    fs::write(root.join("Runner.java"), RUNNER).unwrap();
    let output = Command::new(java)
        .arg(root.join("Runner.java"))
        .arg(&root)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{label}: the woven class did not load or run:\n{}\n{stdout}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout.lines().map(str::to_owned).collect()
}

#[test]
fn plain_classes_round_trip_byte_exactly() {
    let classes = compile("round-trip");
    for name in ["demo/Target", "demo/Callback"] {
        let bytes = read_class(&classes, name);
        let class = ClassFile::parse(&bytes).unwrap();
        assert_eq!(class.write().unwrap(), bytes, "{name} round-trip");
    }
}

#[test]
fn condy_survives_head_and_return_weaving() {
    let classes = compile("weave");
    let original = read_class(&classes, "demo/Target");
    let condied = with_condy(&original);
    let class = ClassFile::parse(&condied).unwrap();
    assert_eq!(
        class.write().unwrap(),
        condied,
        "the injected class is a coherent class file"
    );

    let before_condy = condy_entries(&condied);
    let before_bootstrap = bootstrap_rows(&condied);
    assert_eq!(before_condy.len(), 1, "the fixture has one condy entry");
    assert!(
        before_bootstrap.len() >= 2,
        "concat and the condy each contribute a bootstrap row"
    );

    let plans = [
        MixinInjectionPlan::inject_at_head(
            method(
                "demo/Target",
                "greet",
                "(Ljava/lang/String;)Ljava/lang/String;",
            ),
            method(
                "demo/Callback",
                "onGreet",
                "(Ldemo/Target;Ljava/lang/String;)V",
            ),
        ),
        MixinInjectionPlan::inject_at_return(
            method("demo/Target", "note", "(Ljava/lang/String;)V"),
            method("demo/Callback", "onNote", "(Ljava/lang/String;)V"),
        ),
    ];
    let woven = weave_class(&class, &plans).unwrap();
    let reparsed = ClassFile::parse(&woven).unwrap();
    assert_eq!(
        reparsed.write().unwrap(),
        woven,
        "the woven condy class round-trips"
    );

    // The writer appends constants, so the whole original pool is a prefix and
    // the condy's indices and bootstrap row are untouched.
    let before = split(&condied);
    let after = split(&woven);
    assert!(
        after.slot_count > before.slot_count,
        "the weave appended constants"
    );
    for (index, entry) in before.entries.iter().enumerate() {
        assert_eq!(
            after.entries.get(index),
            Some(entry),
            "constant pool entry {index} moved or changed"
        );
    }
    assert_eq!(condy_entries(&woven), before_condy);
    assert_eq!(bootstrap_rows(&woven), before_bootstrap);

    let (_, bootstrap, name_type) = before_condy[0];
    let slots = pool_slots(&woven);
    let (tag, payload) = slots[name_type as usize]
        .as_ref()
        .expect("the condy name-and-type survives");
    assert_eq!(*tag, 12);
    let name = u16::from_be_bytes([payload[0], payload[1]]);
    let descriptor = u16::from_be_bytes([payload[2], payload[3]]);
    let utf8 = |index: u16| match slots[index as usize].as_ref() {
        Some((1, payload)) => String::from_utf8(payload[2..].to_vec()).unwrap(),
        other => panic!("pool index {index} is not Utf8: {other:?}"),
    };
    assert_eq!(utf8(name), "condyValue");
    assert_eq!(utf8(descriptor), "Ljava/lang/Object;");
    let rows = bootstrap_rows(&woven);
    let (handle, arguments) = &rows[bootstrap as usize];
    assert!(
        arguments.is_empty(),
        "the fixture bootstrap has no arguments"
    );
    assert_eq!(
        slots[*handle as usize].as_ref().map(|(tag, _)| *tag),
        Some(15),
        "the bootstrap method handle survives"
    );
}

/// Every constant-pool slot, indexed by pool index (`None` for the reserved
/// slot and a wide value's second slot).
fn pool_slots(bytes: &[u8]) -> Vec<Option<(u8, Vec<u8>)>> {
    let parts = split(bytes);
    let mut slots = vec![None; parts.slot_count as usize];
    let mut index = 1usize;
    for (tag, payload) in &parts.entries {
        slots[index] = Some((*tag, payload.clone()));
        index += if matches!(tag, 5 | 6) { 2 } else { 1 };
    }
    slots
}

#[test]
fn condy_class_encodes_and_runs_the_constant() {
    if java().is_none() {
        return;
    }
    let classes = compile("run");
    let original = read_class(&classes, "demo/Target");
    let condied = with_condy(&original);
    let class = ClassFile::parse(&condied).unwrap();
    let plans = [
        MixinInjectionPlan::inject_at_head(
            method(
                "demo/Target",
                "greet",
                "(Ljava/lang/String;)Ljava/lang/String;",
            ),
            method(
                "demo/Callback",
                "onGreet",
                "(Ldemo/Target;Ljava/lang/String;)V",
            ),
        ),
        MixinInjectionPlan::inject_at_return(
            method("demo/Target", "note", "(Ljava/lang/String;)V"),
            method("demo/Callback", "onNote", "(Ljava/lang/String;)V"),
        ),
    ];
    let woven = weave_class(&class, &plans).unwrap();

    let baseline = run(&classes, &original, "baseline");
    let actual = run(&classes, &woven, "woven");
    assert_eq!(
        baseline
            .iter()
            .filter(|line| line.starts_with("greet=") || line.starts_with("sink="))
            .collect::<Vec<_>>(),
        actual
            .iter()
            .filter(|line| line.starts_with("greet=") || line.starts_with("sink="))
            .collect::<Vec<_>>(),
        "the callbacks changed the target's behaviour"
    );
    assert!(
        baseline.iter().any(|line| line == "condy=absent"),
        "{baseline:?}"
    );
    assert!(
        actual.iter().any(|line| line == "condy=null"),
        "the woven class must still resolve the condy: {actual:?}"
    );
    assert!(
        actual.iter().any(|line| line == "calls=2"),
        "HEAD and RETURN callbacks each ran once: {actual:?}"
    );
}

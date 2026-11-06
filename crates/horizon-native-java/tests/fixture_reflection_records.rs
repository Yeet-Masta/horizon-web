//! Reflection, record and enum fixtures compiled from real Java at test time.
//!
//! The interpreter advertises a *bounded* reflection surface: `Class.forName` for classes in the
//! current jar, field/method/constructor enumeration markers, `java.lang.reflect.Array`, annotations
//! parsed from the class file, `Field.get`/`set`, primitive-constant unboxing in `Method.invoke`,
//! enum identity for constants a jar's own `<clinit>` built, and a record's generated object methods.
//! These fixtures pin down exactly what a real mod sees, so the suite never claims more than the
//! interpreter does.

use horizon_native_java::{NativeError, NativeJar, StdHost, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};
use zip::write::SimpleFileOptions;

const SOURCE: &str = r#"package demo;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
import java.lang.reflect.Array;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

@Retention(RetentionPolicy.RUNTIME)
@Target({ElementType.TYPE, ElementType.METHOD})
@interface Marker { String value(); }

@Marker("model")
final class Model {
    public static final int LIMIT = 3;
    public final String label;
    private int count;
    public Model() { this("default", 0); }
    public Model(String label, int count) { this.label = label; this.count = count; }
    public static String versionName() { return "1.0"; }
    @Marker("label") public String label() { return label; }
    public int bump(int by) { return count + by; }
}

record Point(int x, int y) { public int sum() { return x + y; } }

/** Instance and static fields a reflective write can reach. */
final class Counter {
    public static int TOTAL = 0;
    private int value = 0;
    public int value() { return value; }
    public static int total() { return TOTAL; }
}

enum Color { RED, GREEN, BLUE }

/** One probe per reflection API a loader/bootstrap mod commonly touches. */
final class Probe {
    private static void marker(String text) { System.getenv(text); }

    public static void onInitialize() throws Exception {
        StringBuilder facts = new StringBuilder();
        facts = facts.append(Class.forName("demo.Model").getDeclaredFields().length).append('|');
        facts = facts.append(Class.forName("demo.Model").getDeclaredMethods().length).append('|');
        facts = facts.append(invokeLabel()).append('|').append(recordAccessors()).append('|');
        facts = facts.append(enumValueOfName()).append('|').append(arrayFacts());
        marker(facts.toString());
    }

    public static String classLookup() throws Exception {
        String missing;
        try {
            missing = Class.forName("demo.Missing").getName();
        } catch (ClassNotFoundException expected) {
            missing = "absent";
        }
        return new StringBuilder().append(Class.forName("demo.Model").getName()).append(',')
            .append(missing).toString();
    }

    public static String fieldFacts() throws Exception {
        Field[] declared = Class.forName("demo.Model").getDeclaredFields();
        Field[] visible = Class.forName("demo.Model").getFields();
        StringBuilder facts = new StringBuilder().append(declared.length).append('|');
        for (int i = 0; i < visible.length; i++) {
            if (i > 0) { facts = facts.append(','); }
            facts = facts.append(visible[i].getName());
        }
        facts = facts.append('|');
        for (int i = 0; i < declared.length; i++) {
            if (i > 0) { facts = facts.append(','); }
            facts = facts.append(declared[i].getName()).append('=');
            facts = facts.append(declared[i].getType().getName()).append(':');
            facts = facts.append(declared[i].getModifiers());
        }
        return facts.toString();
    }

    public static String methodFacts() throws Exception {
        Method[] methods = Class.forName("demo.Model").getDeclaredMethods();
        StringBuilder facts = new StringBuilder().append(methods.length);
        for (int i = 0; i < methods.length; i++) {
            facts = facts.append('|').append(methods[i].toString());
        }
        return facts.toString();
    }

    public static String methodLookup() throws Exception {
        Method method = Class.forName("demo.Model").getMethod("label", new Class[0]);
        return new StringBuilder().append(method.getName()).append(':')
            .append(method.getParameterCount()).toString();
    }

    public static String invokeLabel() throws Exception {
        Method method = Class.forName("demo.Model").getMethod("label", new Class[0]);
        return (String) method.invoke(new Model("reflected", 2), new Object[0]);
    }

    public static String invokeStatic() throws Exception {
        Method method = Class.forName("demo.Model").getMethod("versionName", new Class[0]);
        return (String) method.invoke((Object) null, new Object[0]);
    }

    public static int primitiveLookupParamCount() throws Exception {
        return Class.forName("demo.Model").getMethod("bump", new Class[]{int.class})
            .getParameterCount();
    }

    public static int invokeBump() throws Exception {
        Method method = Class.forName("demo.Model").getMethod("bump", new Class[0]);
        Object result = method.invoke(new Model("boxed", 5), new Object[]{Integer.valueOf(3)});
        return ((Integer) result).intValue();
    }

    public static String fieldValue() throws Exception {
        Field field = Class.forName("demo.Model").getDeclaredField("label");
        return (String) field.get(new Model("read", 4));
    }

    /** A reflective write lands on the instance's field and on a static field. */
    public static String fieldWrite() throws Exception {
        Class<?> type = Class.forName("demo.Counter");
        Counter counter = new Counter();
        Field value = type.getDeclaredField("value");
        value.set(counter, Integer.valueOf(7));
        Field total = type.getDeclaredField("TOTAL");
        total.set(type, Integer.valueOf(5));
        return new StringBuilder().append(counter.value()).append('|')
            .append(Counter.total()).append('|')
            .append(((Integer) value.get(counter)).intValue()).append('|')
            .append(((Integer) total.get(type)).intValue()).toString();
    }

    /** `annotationType()` names the runtime annotation the lookup returned. */
    public static String annotationTypeName() throws Exception {
        Marker found = Class.forName("demo.Model").getAnnotation(Marker.class);
        return found == null ? "absent" : found.annotationType().getName();
    }

    public static String reflectedConstructorLabel() throws Exception {
        Constructor<?> constructor = Class.forName("demo.Model").getDeclaredConstructor(new Class[0]);
        return ((Model) constructor.newInstance(new Object[0])).label();
    }

    public static String recordAccessors() {
        Point point = new Point(3, 4);
        return new StringBuilder().append(point.x()).append(',').append(point.y()).append(',')
            .append(point.sum()).toString();
    }

    public static String recordToString() { return new Point(3, 4).toString(); }
    public static boolean recordEquals() { return new Point(3, 4).equals(new Point(3, 4)); }
    public static int recordHash() { return new Point(3, 4).hashCode(); }
    public static String enumValueOfName() { return ((Enum<?>) Color.valueOf("GREEN")).name(); }
    public static String enumConcreteName() { return Color.valueOf("GREEN").name(); }
    public static int enumConcreteOrdinal() { return Color.valueOf("BLUE").ordinal(); }
    public static int enumSupertypeOrdinal() { return ((Enum<?>) Color.valueOf("BLUE")).ordinal(); }

    public static String enumConstantName() {
        Color.valueOf("RED");
        return ((Enum<?>) Color.RED).name();
    }

    public static int enumValuesLength() { return Color.values().length; }

    public static String classAnnotation() throws Exception {
        Marker found = Class.forName("demo.Model").getAnnotation(Marker.class);
        return found == null ? "absent" : found.value();
    }

    public static String methodAnnotation() throws Exception {
        Method method = Class.forName("demo.Model").getMethod("label", new Class[0]);
        Marker found = method.getAnnotation(Marker.class);
        return found == null ? "absent" : found.value();
    }

    /** A class without `@Marker` must keep Java's `null`, not an empty proxy. */
    public static String missingClassAnnotation() throws Exception {
        Marker found = Class.forName("demo.Probe").getAnnotation(Marker.class);
        return found == null ? "absent" : found.value();
    }

    public static String arrayFacts() throws Exception {
        Object ints = Array.newInstance(int.class, 3);
        Array.setInt(ints, 0, 7);
        Array.setInt(ints, 2, 9);
        int roundTrip = Array.getLength(ints) * 100 + Array.getInt(ints, 0) * 10
            + Array.getInt(ints, 2);
        Object names = Array.newInstance(String.class, 2);
        Array.set(names, 0, "alpha");
        Array.set(names, 1, "beta");
        return new StringBuilder().append(roundTrip).append('|').append(Array.getLength(names))
            .append(':').append((String) Array.get(names, 0)).toString();
    }

    /** A reflective member the jar does not declare must not resolve. */
    public static void missingMember() throws Exception {
        Method method = Class.forName("demo.Model").getDeclaredMethod("nope", new Class[0]);
        method.invoke(new Model(), new Object[0]);
    }
}
"#;

fn javac() -> PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into())
}

fn files_under(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root)
        .unwrap()
        .flat_map(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files_under(&path)
            } else {
                vec![path]
            }
        })
        .collect()
}

/// One compiled fixture jar, removed when the test that built it finishes.
struct Fixture {
    root: PathBuf,
    jar: NativeJar,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    /// Compile the shared Java source into a Fabric-shaped jar.  Each test builds its own jar so
    /// parallel tests never share a scratch directory.
    fn build(test: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "horizon-native-java-reflect-{test}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        // Own the directory before compiling, so a failed javac still cleans up.
        let mut fixture = Self {
            root,
            jar: NativeJar::default(),
        };
        let src = fixture.root.join("src/demo/Fixture.java");
        let classes = fixture.root.join("classes");
        fs::create_dir_all(&classes).unwrap();
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        fs::write(&src, SOURCE).unwrap();
        let status = Command::new(javac())
            .arg("--release")
            .arg("17")
            .arg("-d")
            .arg(&classes)
            .arg(&src)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "javac failed for reflection fixture {test}"
        );

        let jar_path = fixture.root.join("reflection.jar");
        let mut zip = zip::ZipWriter::new(fs::File::create(&jar_path).unwrap());
        zip.start_file("fabric.mod.json", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(
            br#"{"id":"reflection-fixture","version":"1.0","entrypoints":{"main":"demo.Probe"}}"#,
        )
        .unwrap();
        for class in files_under(&classes) {
            let entry = class
                .strip_prefix(&classes)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            zip.start_file(entry, SimpleFileOptions::default()).unwrap();
            zip.write_all(&fs::read(class).unwrap()).unwrap();
        }
        zip.finish().unwrap();

        fixture.jar = NativeJar::from_path(&jar_path).unwrap();
        fixture
    }

    fn call(&self, method: &str, descriptor: &str) -> Result<Value, NativeError> {
        self.jar
            .invoke_method("demo/Probe", method, descriptor, None, &mut StdHost::new())
    }

    fn text(&self, method: &str) -> String {
        match self.call(method, "()Ljava/lang/String;").unwrap() {
            Value::Ref(Some(text)) => text,
            other => panic!("{method} did not return a string: {other:?}"),
        }
    }

    fn number(&self, method: &str) -> i32 {
        match self.call(method, "()I").unwrap() {
            Value::Int(value) => value,
            other => panic!("{method} did not return an int: {other:?}"),
        }
    }
}

#[test]
fn runtime_built_fixture_bootstraps_through_reflection() {
    let fixture = Fixture::build("bootstrap");
    assert_eq!(
        fixture.jar.discover_entrypoint("main").unwrap().class,
        "demo.Probe"
    );
    let mut host = StdHost::new();
    fixture
        .jar
        .execute("demo.Probe", Some("onInitialize"), "main", &mut host)
        .unwrap();
    let getenv = horizon_native_java::MethodRef {
        owner: "java/lang/System".into(),
        name: "getenv".into(),
        descriptor: "(Ljava/lang/String;)Ljava/lang/String;".into(),
    };
    let markers: Vec<_> = host
        .invocations()
        .iter()
        .filter(|invocation| invocation.method == getenv)
        .filter_map(|invocation| invocation.args.first().cloned())
        .collect();
    // fields | methods | reflected label | record accessors | enum name | int + String arrays
    assert_eq!(
        markers,
        vec![Value::Ref(Some(
            "3|3|reflected|3,4,7|GREEN|379|2:alpha".into()
        ))]
    );
}

#[test]
fn class_for_name_fields_and_methods_use_local_markers() {
    let fixture = Fixture::build("surface");
    assert_eq!(fixture.text("classLookup"), "demo.Model,absent");
    assert_eq!(
        fixture.text("fieldFacts"),
        "3|LIMIT,label|LIMIT=int:25,label=java.lang.String:17,count=int:2"
    );
    assert_eq!(
        fixture.text("methodFacts"),
        concat!(
            "3",
            "|reflect-method/demo/Model#versionName()Ljava/lang/String;",
            "|reflect-method/demo/Model#label()Ljava/lang/String;",
            "|reflect-method/demo/Model#bump(I)I"
        )
    );
    assert_eq!(fixture.text("methodLookup"), "label:0");
}

#[test]
fn method_invoke_dispatches_instance_and_static_members() {
    let fixture = Fixture::build("invoke");
    assert_eq!(fixture.text("invokeLabel"), "reflected");
    assert_eq!(fixture.text("invokeStatic"), "1.0");
}

#[test]
fn record_accessors_and_explicit_methods_run_as_bytecode() {
    assert_eq!(Fixture::build("record").text("recordAccessors"), "3,4,7");
}

#[test]
fn enum_supertype_name_and_ordinal_follow_the_declaration() {
    let fixture = Fixture::build("enum");
    // `Enum<?>` receivers reach the modelled `java.lang.Enum` surface, which now
    // answers the declared name and position of the constants `<clinit>` built.
    assert_eq!(fixture.text("enumValueOfName"), "GREEN");
    assert_eq!(fixture.number("enumSupertypeOrdinal"), 2);
    assert_eq!(fixture.text("enumConstantName"), "RED");
}

#[test]
fn reflective_arrays_round_trip_ints_and_strings() {
    assert_eq!(Fixture::build("arrays").text("arrayFacts"), "379|2:alpha");
}

#[test]
fn annotation_lookup_reports_absence_for_a_class_without_the_annotation() {
    let fixture = Fixture::build("absence");
    // `Probe` declares no `@Marker`, so the query keeps Java's `null` instead of
    // handing out an invented proxy.
    assert_eq!(fixture.text("missingClassAnnotation"), "absent");
}

#[test]
fn unknown_reflective_member_fails_with_a_bounded_diagnostic() {
    let error = Fixture::build("missing")
        .call("missingMember", "()V")
        .unwrap_err();
    let diagnostic = error.to_string();
    assert!(
        matches!(&error, NativeError::MissingMethod(member) if member == "demo/Model#nope"),
        "unexpected diagnostic: {diagnostic}"
    );
    // The diagnostic names both the class and the member, so a loader can report which
    // reflective call failed instead of panicking.
    assert!(diagnostic.contains("demo/Model#nope"), "{diagnostic}");
}

#[test]
fn record_generated_object_methods_match_java() {
    let fixture = Fixture::build("record-java");
    assert_eq!(fixture.text("recordToString"), "Point[x=3, y=4]");
    assert_eq!(fixture.call("recordEquals", "()Z").unwrap(), Value::Int(1));
    // `Point(3, 4)` hashes to 31 * (31 * 1 + 3) + 4 in the JVM.
    assert_eq!(fixture.call("recordHash", "()I").unwrap(), Value::Int(1058));
}

#[test]
fn enum_ordinal_reports_the_declared_position() {
    assert_eq!(
        Fixture::build("enum-ordinal").number("enumSupertypeOrdinal"),
        2
    );
}

#[test]
fn enum_inherited_members_resolve_through_the_supertype() {
    let fixture = Fixture::build("enum-inherited");
    assert_eq!(fixture.text("enumConcreteName"), "GREEN");
    assert_eq!(fixture.number("enumConcreteOrdinal"), 2);
}

#[test]
fn enum_constant_name_is_the_declared_name() {
    assert_eq!(Fixture::build("enum-name").text("enumConstantName"), "RED");
}

#[test]
fn enum_values_returns_the_declared_constants() {
    assert_eq!(Fixture::build("enum-values").number("enumValuesLength"), 3);
}

#[test]
fn annotations_are_visible_to_the_mod() {
    let fixture = Fixture::build("annotations");
    assert_eq!(fixture.text("classAnnotation"), "model");
    assert_eq!(fixture.text("methodAnnotation"), "label");
    // The proxy answers its own type, so `annotationType().getName()` is the
    // interface the class file declared.
    assert_eq!(fixture.text("annotationTypeName"), "demo.Marker");
}

#[test]
fn field_write_answers_instance_and_static_fields() {
    assert_eq!(Fixture::build("field-write").text("fieldWrite"), "7|5|7|5");
}

#[test]
fn reflected_constructor_creates_an_instance() {
    assert_eq!(
        Fixture::build("constructor").text("reflectedConstructorLabel"),
        "default"
    );
}

#[test]
fn field_read_returns_the_declared_value() {
    assert_eq!(Fixture::build("field-read").text("fieldValue"), "read");
}

#[test]
fn method_lookup_keeps_the_primitive_parameter() {
    assert_eq!(
        Fixture::build("primitive-lookup").number("primitiveLookupParamCount"),
        1
    );
}

#[test]
fn method_invoke_unboxes_primitive_arguments() {
    assert_eq!(Fixture::build("boxed-invoke").number("invokeBump"), 8);
}

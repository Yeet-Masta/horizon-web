//! Generic reflection fixtures compiled from real Java at test time.
//!
//! `Field.getGenericType` reads the field's class-file `Signature` attribute
//! and answers the four `java.lang.reflect.Type` shapes the bounded runtime
//! models: a class marker for a plain type, a parameterized marker with a raw
//! type and type arguments, a type variable, and a wildcard. The predicates
//! here mirror what a real mod does before it accepts a field — comparing the
//! raw type with `List.class` — so this test pins the branch a mod takes, not
//! just the marker text.

use horizon_native_java::{NativeJar, StdHost, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};
use zip::write::SimpleFileOptions;

const SOURCE: &str = r#"package demo;

import java.lang.reflect.Field;
import java.lang.reflect.ParameterizedType;
import java.lang.reflect.Type;
import java.lang.reflect.TypeVariable;
import java.lang.reflect.WildcardType;
import java.util.List;
import java.util.Map;
import java.util.function.BiPredicate;

final class Generics {
    public static final String PLAIN = "plain";
    public List<String> names;
    public Map<String, List<Integer>> grouped;
    public List<? extends Number> numbers;
    public List<? super Integer> sink;
    public String[] array;
    public int count;

    static class Outer<T> {
        class Inner<U> {
            public T outer;
            public U inner;
        }
        public Outer<String>.Inner<Integer> nested;
    }
}

final class Probe {
    /** The mod-shaped predicate: accept a field whose raw type is a list. */
    private static final BiPredicate<Object, Type> LIST = (type, generic) ->
        type.equals(java.util.ArrayList.class) || type.equals(List.class);

    private static String describeType(Type type) {
        if (type instanceof Class<?> klass) {
            return "C" + klass.getSimpleName();
        }
        if (type instanceof TypeVariable<?> variable) {
            Type[] bounds = variable.getBounds();
            boolean object = bounds.length == 1 && bounds[0] == Object.class;
            return "V" + variable.getName() + (object ? "" : "?");
        }
        if (type instanceof WildcardType wildcard) {
            Type[] upper = wildcard.getUpperBounds();
            Type[] lower = wildcard.getLowerBounds();
            if (lower.length > 0) {
                return "Wsuper " + lower[0].getTypeName();
            }
            return "W" + upper[0].getTypeName();
        }
        if (type instanceof ParameterizedType nested) {
            StringBuilder out = new StringBuilder("P");
            for (Type arg : nested.getActualTypeArguments()) {
                out.append(describeType(arg));
            }
            return out.toString();
        }
        return "?";
    }

    private static String describe(String fieldName) throws Exception {
        Field field = Generics.class.getField(fieldName);
        Type generic = field.getGenericType();
        StringBuilder out = new StringBuilder();
        out.append(field.getType() == List.class ? "L" : "-");
        out.append(LIST.test(field.getType(), generic) ? "p" : "n");
        if (generic instanceof ParameterizedType parameterized) {
            out.append("P:");
            Type raw = parameterized.getRawType();
            out.append(raw == List.class ? "list" : raw == Map.class ? "map" : String.valueOf(raw));
            Type[] args = parameterized.getActualTypeArguments();
            out.append(':').append(args.length);
            for (Type arg : args) {
                out.append(',').append(describeType(arg));
            }
            out.append(':').append(parameterized.getOwnerType() == null ? "null" : "owner");
        } else {
            out.append("N:");
            out.append(generic == String.class ? "string" : "other");
        }
        return out.toString();
    }

    public static String genericFacts() throws Exception {
        StringBuilder out = new StringBuilder();
        out.append(describe("names")).append('|');
        out.append(describe("grouped")).append('|');
        out.append(describe("numbers")).append('|');
        out.append(describe("sink")).append('|');
        out.append(describe("count")).append('|');
        out.append(describe("array"));
        return out.toString();
    }

    public static String ownerFacts() throws Exception {
        Field field = Generics.Outer.class.getField("nested");
        ParameterizedType type = (ParameterizedType) field.getGenericType();
        Type raw = type.getRawType();
        ParameterizedType owner = (ParameterizedType) type.getOwnerType();
        return (raw == Generics.Outer.Inner.class ? "inner" : "?")
            + "|" + (owner.getRawType() == Generics.Outer.class ? "outer" : "?")
            + "|" + owner.getActualTypeArguments().length
            + "|" + ((Class<?>) type.getActualTypeArguments()[0]).getSimpleName()
            + "|" + type.getTypeName();
    }

    public static String typeVariableFacts() throws Exception {
        Field field = Generics.Outer.Inner.class.getField("inner");
        Type generic = field.getGenericType();
        TypeVariable<?> variable = (TypeVariable<?>) generic;
        return variable.getName()
            + "|" + (variable.getBounds()[0] == Object.class ? "object" : "?")
            + "|" + (generic instanceof TypeVariable ? "var" : "?")
            + "|" + (generic instanceof ParameterizedType ? "param" : "plain");
    }

    public static String plainFacts() throws Exception {
        Field plain = Generics.class.getField("PLAIN");
        return (plain.getGenericType() == String.class ? "same" : "different")
            + "|" + (plain.getGenericType() instanceof Class ? "class" : "?");
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
    fn build(test: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "horizon-native-java-generics-{test}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
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
            .args(["--release", "17", "-d"])
            .arg(&classes)
            .arg(&src)
            .status()
            .unwrap();
        assert!(status.success(), "javac failed for generics fixture {test}");

        let jar_path = fixture.root.join("generics.jar");
        let mut zip = zip::ZipWriter::new(fs::File::create(&jar_path).unwrap());
        zip.start_file("fabric.mod.json", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(
            br#"{"id":"generics-fixture","version":"1.0","entrypoints":{"main":"demo.Probe"}}"#,
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

    fn text(&self, method: &str) -> String {
        let value = self
            .jar
            .invoke_method(
                "demo/Probe",
                method,
                "()Ljava/lang/String;",
                None,
                &mut StdHost::new(),
            )
            .unwrap();
        match value {
            Value::Ref(Some(text)) => text,
            other => panic!("{method} did not return a string: {other:?}"),
        }
    }
}

#[test]
fn field_generic_types_answer_parameterized_variables_and_wildcards() {
    let fixture = Fixture::build("surface");
    assert_eq!(
        fixture.text("genericFacts"),
        concat!(
            "LpP:list:1,CString:null",
            "|-nP:map:2,CString,PCInteger:null",
            "|LpP:list:1,Wjava.lang.Number:null",
            "|LpP:list:1,Wsuper java.lang.Integer:null",
            "|-nN:other",
            "|-nN:other",
        )
    );
}

#[test]
fn a_parameterized_nested_type_reports_its_owner() {
    let fixture = Fixture::build("owner");
    assert_eq!(
        fixture.text("ownerFacts"),
        "inner|outer|1|Integer|demo.Generics$Outer$Inner<java.lang.Integer>"
    );
}

#[test]
fn a_type_variable_reports_its_name_bounds_and_shape() {
    let fixture = Fixture::build("variable");
    assert_eq!(fixture.text("typeVariableFacts"), "U|object|var|plain");
}

#[test]
fn a_plain_field_generic_type_is_the_class_itself() {
    let fixture = Fixture::build("plain");
    assert_eq!(fixture.text("plainFacts"), "same|class");
}

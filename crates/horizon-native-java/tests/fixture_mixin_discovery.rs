//! Runtime-built Mixin fixture for annotation-driven injection discovery.
//!
//! The class files come from `javac` so the test exercises the real
//! `RuntimeVisibleAnnotations` encoding instead of hand-written attribute
//! bytes.

use horizon_native_java::{AnnotationValue, ClassFile, NativeJar};
use std::{collections::BTreeMap, fs, io::Write, path::Path, process::Command};
use zip::write::SimpleFileOptions;

const MIXIN: &str = "Lorg/spongepowered/asm/mixin/Mixin;";
const INJECT: &str = "Lorg/spongepowered/asm/mixin/injection/Inject;";

fn javac() -> std::path::PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| std::path::PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into())
}

fn files_under(root: &Path) -> Vec<std::path::PathBuf> {
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

fn build_jar(
    name: &str,
    sources: BTreeMap<&str, &str>,
    metadata_path: &str,
    metadata: &[u8],
) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-mixin-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    let classes = root.join("classes");
    fs::create_dir_all(&classes).unwrap();
    let mut paths = Vec::new();
    for (relative, body) in sources {
        let path = src.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, body).unwrap();
        paths.push(path);
    }
    let status = Command::new(javac())
        .arg("--release")
        .arg("17")
        .arg("-d")
        .arg(&classes)
        .args(paths)
        .status()
        .unwrap();
    assert!(status.success(), "javac failed for {name}");

    let jar_path = root.join(format!("{name}.jar"));
    let mut zip = zip::ZipWriter::new(fs::File::create(&jar_path).unwrap());
    zip.start_file(metadata_path, SimpleFileOptions::default())
        .unwrap();
    zip.write_all(metadata).unwrap();
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
    (root, jar_path)
}

#[test]
fn discovers_supported_inject_plans_and_keeps_annotations() {
    let mut sources = BTreeMap::new();
    sources.insert(
        "org/spongepowered/asm/mixin/Mixin.java",
        r#"package org.spongepowered.asm.mixin;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.TYPE)
public @interface Mixin {
    Class<?>[] value() default {};
    String[] targets() default {};
}
"#,
    );
    sources.insert(
        "org/spongepowered/asm/mixin/injection/At.java",
        r#"package org.spongepowered.asm.mixin.injection;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target({})
public @interface At {
    String value() default "";
}
"#,
    );
    sources.insert(
        "org/spongepowered/asm/mixin/injection/Inject.java",
        r#"package org.spongepowered.asm.mixin.injection;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.METHOD)
public @interface Inject {
    String[] method();
    At[] at();
    boolean cancellable() default false;
}
"#,
    );
    sources.insert(
        "demo/Target.java",
        r#"package demo;
public final class Target {
    public void tick() {}
    public int value() { return 1; }
}
"#,
    );
    sources.insert(
        "demo/TargetMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;

@Mixin(value = Target.class, targets = {"Ldemo/Target;"})
public abstract class TargetMixin {
    @Inject(method = "tick", at = @At("HEAD"))
    private void onTickHead() {}

    @Inject(method = "value()I", at = @At("RETURN"), cancellable = true)
    private void onValueReturn() {}

    @Inject(method = "tick", at = @At("INVOKE"))
    private void onTickInvoke() {}
}
"#,
    );

    let (root, jar_path) = build_jar(
        "discovery",
        sources,
        "fabric.mod.json",
        br#"{"id":"mixin-discovery-fixture","version":"1.0"}"#,
    );

    // Assert the annotation model itself survived class parsing, not just the
    // derived plans.
    let class_bytes = fs::read(root.join("classes/demo/TargetMixin.class")).unwrap();
    let parsed = ClassFile::parse(&class_bytes).unwrap();
    let mixin = parsed
        .annotations
        .iter()
        .find(|annotation| annotation.descriptor == MIXIN)
        .expect("class-level @Mixin annotation survives parsing");
    assert_eq!(
        mixin.values.get("value"),
        Some(&AnnotationValue::Array(vec![AnnotationValue::String(
            "Ldemo/Target;".into()
        )]))
    );
    assert_eq!(
        mixin.values.get("targets"),
        Some(&AnnotationValue::Array(vec![AnnotationValue::String(
            "Ldemo/Target;".into()
        )]))
    );
    let head_method = parsed
        .methods
        .iter()
        .find(|method| method.name == "onTickHead")
        .expect("callback method is present");
    assert!(
        head_method
            .annotations
            .iter()
            .any(|annotation| annotation.descriptor == INJECT),
        "method-level @Inject annotation survives parsing"
    );

    let jar = NativeJar::from_path(&jar_path).unwrap();
    let plans = jar.mixin_injection_plans();

    // `value` and `targets` describe the same owner, so discovery deduplicates
    // them.  The unsupported INVOKE anchor is not represented at all.
    assert_eq!(plans.len(), 2, "unexpected plans: {plans:#?}");
    assert_eq!(
        plans
            .iter()
            .map(|plan| (plan.target.name.as_str(), plan.at.as_str()))
            .collect::<Vec<_>>(),
        vec![("tick", "HEAD"), ("value", "RETURN")]
    );
    assert!(
        plans
            .iter()
            .all(|plan| plan.callback.name != "onTickInvoke"),
        "unsupported @At values do not produce plans"
    );

    let head = &plans[0];
    assert_eq!(head.target.owner, "demo/Target");
    assert_eq!(head.target.name, "tick");
    assert_eq!(head.target.descriptor, "");
    assert_eq!(head.callback.owner, "demo/TargetMixin");
    assert_eq!(head.callback.name, "onTickHead");
    assert_eq!(head.callback.descriptor, "()V");
    assert_eq!(head.at, "HEAD");
    assert!(!head.cancellable);

    let returned = &plans[1];
    assert_eq!(returned.target.owner, "demo/Target");
    assert_eq!(returned.target.name, "value");
    assert_eq!(returned.target.descriptor, "()I");
    assert_eq!(returned.callback.owner, "demo/TargetMixin");
    assert_eq!(returned.callback.name, "onValueReturn");
    assert_eq!(returned.callback.descriptor, "()V");
    assert_eq!(returned.at, "RETURN");
    assert!(returned.cancellable);

    let _ = fs::remove_dir_all(root);
}

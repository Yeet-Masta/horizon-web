use horizon_native_java::{
    ClassFile, MethodRef, MixinInjectionPlan, NativeJar, StdHost, mixin_plans_in_weave_order,
    weave_class_with_extras, weave_classes_with_extras,
};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

/// Class-parse warnings the `--mixin-plans` report prints. The jar keeps more
/// than this row count only to answer the total; a hostile jar must not turn
/// the report into a megabyte of repeated attribute names.
const MAX_REPORTED_CLASS_ANNOTATION_WARNINGS: usize = 16;

/// Command-line shape: `--json`, `--mixin-plans` and `--weave <out.jar>` may
/// appear anywhere, the remaining arguments are the jar, an optional loader
/// kind, and an optional explicit class.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cli {
    json: bool,
    mixin_plans: bool,
    /// `--weave <out.jar>`: write a woven copy of the jar instead of running it.
    weave: Option<String>,
    path: String,
    kind: Option<String>,
    class: Option<String>,
}

impl Cli {
    fn parse(args: impl IntoIterator<Item = String>) -> Option<Self> {
        let mut json = false;
        let mut mixin_plans = false;
        let mut weave = None;
        let mut positional = Vec::new();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            if arg == "--json" {
                json = true;
            } else if arg == "--mixin-plans" {
                mixin_plans = true;
            } else if arg == "--weave" {
                // The output path is the flag's value, never a positional.
                weave = Some(args.next()?);
            } else {
                positional.push(arg);
            }
        }
        let mut positional = positional.into_iter();
        Some(Self {
            json,
            mixin_plans,
            weave,
            path: positional.next()?,
            kind: positional.next(),
            class: positional.next(),
        })
    }
}

/// One jar entry kept in memory so the woven copy can reproduce it exactly.
struct JarEntry {
    name: String,
    data: Vec<u8>,
    /// The class binary name this entry holds, when the native path loads it.
    class: Option<String>,
    /// Whether the entry was stored uncompressed, kept so the copy does not
    /// change how the jar is packaged.
    stored: bool,
    /// Directory entry (`name` ends in `/`); copied with no bytes.
    directory: bool,
}

/// What `--weave` did, and the only thing a caller reads back.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WeaveReport {
    /// Classes whose bytes changed because at least one plan applied.
    woven_classes: usize,
    /// Classes copied byte-identically because no plan reached them.
    untouched_classes: usize,
    /// Plans the bounded weaver applied.
    woven_plans: usize,
    /// Plans it rejected, or that target a class this jar does not contain.
    skipped_plans: usize,
    /// Synthetic classes the woven bytecode references, added to the jar.
    added_classes: usize,
    /// The jar that was written.
    out: String,
}

impl WeaveReport {
    /// `ok` only once a plan actually reached a class; a jar whose plans the
    /// bounded weaver cannot apply is a reportable failure, not a silent copy.
    fn status(&self) -> &'static str {
        if self.woven_classes > 0 {
            "ok"
        } else {
            "error"
        }
    }

    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "status": self.status(),
            "woven_classes": self.woven_classes,
            "woven_plans": self.woven_plans,
            "skipped_plans": self.skipped_plans,
            "added_classes": self.added_classes,
            "out": self.out,
        })
    }
}

/// Bounds on the jar `--weave` copies, so a crafted input cannot exhaust memory.
const MAX_WEAVE_ENTRIES: usize = 65_536;
const MAX_WEAVE_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_WEAVE_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// True for the class entries [`NativeJar::from_path`] loads: the native path
/// skips module descriptors, package markers and multi-release copies.
fn is_native_class(name: &str) -> bool {
    name.ends_with(".class")
        && !name.ends_with("module-info.class")
        && !name.ends_with("package-info.class")
        && !name.starts_with("META-INF/versions/")
}

/// Read every entry of `input` and parse the classes the native path loads.
fn read_jar(input: &Path) -> Result<(Vec<JarEntry>, BTreeMap<String, ClassFile>), String> {
    let file =
        File::open(input).map_err(|error| format!("cannot read {}: {error}", input.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| format!("{} is not a readable jar: {error}", input.display()))?;
    if archive.len() > MAX_WEAVE_ENTRIES {
        return Err(format!(
            "{} has {} entries; the bounded weaver copies at most {MAX_WEAVE_ENTRIES}",
            input.display(),
            archive.len()
        ));
    }
    let mut entries = Vec::with_capacity(archive.len());
    let mut classes = BTreeMap::new();
    let mut total = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("{}: entry {index}: {error}", input.display()))?;
        let name = entry.name().to_owned();
        if name.ends_with('/') {
            entries.push(JarEntry {
                name,
                data: Vec::new(),
                class: None,
                stored: true,
                directory: true,
            });
            continue;
        }
        let size = entry.size();
        if size > MAX_WEAVE_ENTRY_BYTES || total.saturating_add(size) > MAX_WEAVE_TOTAL_BYTES {
            return Err(format!(
                "{}: entry {name} is {} bytes, past the bounded weave limits",
                input.display(),
                size
            ));
        }
        let stored = entry.compression() == zip::CompressionMethod::Stored;
        let mut data = Vec::with_capacity(size as usize);
        entry
            .read_to_end(&mut data)
            .map_err(|error| format!("{}: entry {name}: {error}", input.display()))?;
        total = total.saturating_add(data.len() as u64);
        let class = if is_native_class(&name) {
            let class = ClassFile::parse(&data)
                .map_err(|error| format!("{}: entry {name}: {error}", input.display()))?;
            let class_name = class.name.clone();
            classes.entry(class_name.clone()).or_insert(class);
            Some(class_name)
        } else {
            None
        };
        entries.push(JarEntry {
            name,
            data,
            class,
            stored,
            directory: false,
        });
    }
    Ok((entries, classes))
}

/// Weave one class, dropping the plans the bounded weaver rejects.
///
/// Returns the class bytes when at least one plan applied, how many did, and the
/// synthetic classes the rewrite references.  A plan the weaver cannot apply
/// (`INVOKE`, cancelling a `void` target, an unreadable target method) is a
/// diagnostic, never a reason to abandon the jar.
fn weave_one(
    classes: &BTreeMap<String, ClassFile>,
    owner: &str,
    plans: &[&MixinInjectionPlan],
) -> (Option<Vec<u8>>, usize, BTreeMap<String, Vec<u8>>) {
    let class = classes[owner].clone();
    let owned = plans.iter().map(|plan| (*plan).clone()).collect::<Vec<_>>();
    if let Ok((bytes, added)) = weave_class_with_extras(&class, &owned) {
        return (Some(bytes), owned.len(), added);
    }
    // The class as a whole was rejected, so find the plans it accepts, in the
    // order `weave_class` applies them: HEAD first, then RETURN/TAIL.
    let mut ordered = plans.to_vec();
    ordered.sort_by_key(|plan| plan.at == "RETURN" || plan.at == "TAIL");
    let mut current = class;
    let mut woven = None;
    let mut applied = 0;
    let mut added = BTreeMap::new();
    for plan in ordered {
        let Ok((bytes, extras)) = weave_class_with_extras(&current, std::slice::from_ref(plan))
        else {
            continue;
        };
        let Ok(next) = ClassFile::parse(&bytes) else {
            break;
        };
        current = next;
        woven = Some(bytes);
        applied += 1;
        added.extend(extras);
    }
    (woven, applied, added)
}

/// Write `entries` to `output`, swapping in woven bytes for the classes a plan
/// reached and copying everything else byte for byte.
///
/// `plans` are in the order their callbacks should run; applying them in the
/// order the weaver needs is this function's job.
fn write_woven_jar(
    output: &Path,
    entries: &[JarEntry],
    classes: &BTreeMap<String, ClassFile>,
    plans: &[MixinInjectionPlan],
) -> Result<WeaveReport, String> {
    let plans = mixin_plans_in_weave_order(plans);
    let mut by_owner: BTreeMap<&str, Vec<&MixinInjectionPlan>> = BTreeMap::new();
    let mut skipped_plans = 0;
    for plan in &plans {
        if classes.contains_key(plan.target.owner.as_str()) {
            by_owner
                .entry(plan.target.owner.as_str())
                .or_default()
                .push(plan);
        } else {
            skipped_plans += 1;
        }
    }
    let mut woven: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut added: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut woven_plans = 0;
    let resolved = by_owner
        .values()
        .flatten()
        .map(|plan| (*plan).clone())
        .collect::<Vec<_>>();
    if !resolved.is_empty() {
        // The common case: the weaver applies every plan in one pass.  When it
        // rejects one, isolate per class and then per plan so the jar is
        // written with what it could apply instead of nothing at all.
        match weave_classes_with_extras(classes, &resolved) {
            Ok(output) => {
                for owner in by_owner.keys() {
                    if let Some(class_bytes) = output.classes.get(*owner) {
                        woven.insert((*owner).to_owned(), class_bytes.clone());
                    }
                }
                added = output.added;
                woven_plans = resolved.len();
            }
            Err(_) => {
                for (owner, owner_plans) in &by_owner {
                    let (class_bytes, applied, extras) = weave_one(classes, owner, owner_plans);
                    woven_plans += applied;
                    skipped_plans += owner_plans.len() - applied;
                    if let Some(class_bytes) = class_bytes {
                        woven.insert((*owner).to_owned(), class_bytes);
                    }
                    added.extend(extras);
                }
            }
        }
    }
    let extra = added
        .into_iter()
        .filter(|(name, _)| {
            // A jar that declares the class keeps its own copy; only an entry
            // whose internal name differs could collide, and such a jar is
            // malformed enough to prefer its own bytes.
            !entries
                .iter()
                .any(|entry| entry.name == format!("{name}.class"))
        })
        .map(|(name, data)| JarEntry {
            name: format!("{name}.class"),
            data,
            class: Some(name),
            stored: false,
            directory: false,
        })
        .collect::<Vec<_>>();
    let added_classes = extra.len();

    let file = File::create(output)
        .map_err(|error| format!("cannot write {}: {error}", output.display()))?;
    let mut archive = zip::ZipWriter::new(file);
    for entry in entries.iter().chain(extra.iter()) {
        let fail = |error: zip::result::ZipError| {
            format!(
                "cannot write {} into {}: {error}",
                entry.name,
                output.display()
            )
        };
        let options = zip::write::SimpleFileOptions::default().compression_method(
            if entry.stored || entry.directory {
                zip::CompressionMethod::Stored
            } else {
                zip::CompressionMethod::Deflated
            },
        );
        if entry.directory {
            archive
                .add_directory(entry.name.as_str(), options)
                .map_err(fail)?;
            continue;
        }
        let bytes = entry
            .class
            .as_ref()
            .and_then(|name| woven.get(name))
            .map_or(entry.data.as_slice(), Vec::as_slice);
        archive
            .start_file(entry.name.as_str(), options)
            .map_err(fail)?;
        archive.write_all(bytes).map_err(|error| {
            format!(
                "cannot write {} into {}: {error}",
                entry.name,
                output.display()
            )
        })?;
    }
    archive
        .finish()
        .map_err(|error| format!("cannot finish {}: {error}", output.display()))?;
    let untouched_classes = entries
        .iter()
        .filter(|entry| {
            entry
                .class
                .as_ref()
                .is_some_and(|name| !woven.contains_key(name))
        })
        .count();
    Ok(WeaveReport {
        woven_classes: woven.len(),
        untouched_classes,
        woven_plans,
        skipped_plans,
        added_classes,
        out: output.display().to_string(),
    })
}

/// Copy `input` to `output`, weaving every Mixin plan the bounded weaver can
/// apply and leaving every other entry untouched.
///
/// Plan discovery is the same one `--mixin-plans` reports, so a plan the
/// loader would execute is exactly a plan this copies into the jar.  Plans the
/// weaver rejects stay in the report as `skipped_plans`; the jar is still
/// written.  A status of `ok` needs at least one woven class, and the output is
/// the only file this ever creates.
fn weave_jar(input: &Path, output: &Path) -> Result<WeaveReport, String> {
    if output.exists() {
        if let (Ok(source), Ok(target)) =
            (std::fs::canonicalize(input), std::fs::canonicalize(output))
        {
            if source == target {
                return Err(format!(
                    "refusing to weave {} onto itself; pass a different output jar",
                    input.display()
                ));
            }
        }
    }
    let (entries, classes) = read_jar(input)?;
    // From the jar rather than its classes alone: the callback order depends on
    // the Mixin configs, which only the metadata read sees.
    let plans = NativeJar::from_path(input)
        .map_err(|error| format!("{}: {error}", input.display()))?
        .mixin_plans_in_apply_order();
    write_woven_jar(output, &entries, &classes, &plans)
}

/// `--weave <out.jar>`: write the woven jar and report what reached it.
fn weave_main(input: &str, output: &str, json: bool) -> ExitCode {
    let report = match weave_jar(Path::new(input), Path::new(output)) {
        Ok(report) => report,
        Err(error) => {
            println!(
                "{}",
                serde_json::json!({"status": "error", "stage": "weave", "error": error})
            );
            eprintln!("weave failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    if !json {
        println!(
            "woven {} plan(s) into {} class(es); {} class(es) had no plan, {} plan(s) skipped, {} class(es) added -> {}",
            report.woven_plans,
            report.woven_classes,
            report.untouched_classes,
            report.skipped_plans,
            report.added_classes,
            report.out
        );
    }
    println!("{}", report.json());
    if report.woven_classes == 0 {
        if report.skipped_plans > 0 {
            eprintln!(
                "weave wrote {} without applying a plan; the bounded weaver rejected all {} plan(s)",
                report.out, report.skipped_plans
            );
        } else {
            eprintln!(
                "weave wrote {} without applying a plan; {} declares no Mixin plan the bounded weaver can apply",
                report.out, input
            );
        }
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Render a method as `owner#name(descriptor)`.
///
/// The descriptor keeps its own parentheses, so a concrete method reads
/// `demo/Target#greet(Ljava/lang/String;)Ljava/lang/String;` and a
/// descriptor-less `@Inject(method = "greet")` reads `demo/Target#greet`.
fn method_label(method: &MethodRef) -> String {
    format!("{}#{}{}", method.owner, method.name, method.descriptor)
}

/// Sorted, deduplicated `owner#name(descriptor)` labels for target methods.
fn mixin_target_labels<'a>(targets: impl IntoIterator<Item = &'a MethodRef>) -> Vec<String> {
    let mut labels = targets.into_iter().map(method_label).collect::<Vec<_>>();
    labels.sort();
    labels.dedup();
    labels
}

/// Sort key for the plan report, mirroring the discovery order.
fn mixin_plan_sort_key(
    plan: &MixinInjectionPlan,
) -> (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    bool,
) {
    (
        plan.target.owner.clone(),
        plan.target.name.clone(),
        plan.target.descriptor.clone(),
        plan.at.clone(),
        plan.callback.owner.clone(),
        plan.callback.name.clone(),
        plan.callback.descriptor.clone(),
        plan.invoke_target
            .clone()
            .map_or_else(String::new, |target| method_label(&target)),
        plan.cancellable,
    )
}

/// Report rows for discovered plans, sorted and deduplicated.
///
/// `callback` carries the mixin class and method (`owner#name`) because a
/// callback only makes sense together with the class that declares it;
/// `callback_descriptor` holds the rest of its signature.
fn mixin_plan_entries(plans: &[MixinInjectionPlan]) -> Vec<serde_json::Value> {
    let mut plans = plans.to_vec();
    plans.sort_by_key(mixin_plan_sort_key);
    plans.dedup();
    plans
        .iter()
        .map(|plan| {
            serde_json::json!({
                "target": plan.target.owner,
                "method": plan.target.name,
                "descriptor": plan.target.descriptor,
                "callback": format!("{}#{}", plan.callback.owner, plan.callback.name),
                "callback_descriptor": plan.callback.descriptor,
                "at": plan.at,
                "invoke_target": plan.invoke_target.as_ref().map(method_label),
                "cancellable": plan.cancellable,
            })
        })
        .collect()
}

/// The `--mixin-plans` report: planning only, no entrypoint is required.
fn mixin_plans_json(jar: &NativeJar) -> serde_json::Value {
    let plans = jar.mixin_injection_plans();
    let unresolved = jar.unresolved_invoke_plans();
    let class_warnings = jar.class_annotation_warnings();
    let configs = jar
        .mixin_configs()
        .iter()
        .map(|config| {
            serde_json::json!({
                "path": config.path,
                "package": config.package,
                "mixins": config.mixins,
                "client": config.client,
                "server": config.server,
                "required": config.required,
                "priority": config.priority,
                "mixin_priority": config.mixin_priority,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "status": "ok",
        "targets": mixin_target_labels(plans.iter().map(|plan| &plan.target)),
        "plans": mixin_plan_entries(&plans),
        "unresolved": mixin_plan_entries(&unresolved),
        "configs": configs,
        "config_errors": jar
            .mixin_config_errors()
            .iter()
            .map(|(path, reason)| serde_json::json!([path, reason]))
            .collect::<Vec<_>>(),
        "class_annotation_warnings": class_warnings
            .iter()
            .take(MAX_REPORTED_CLASS_ANNOTATION_WARNINGS)
            .cloned()
            .collect::<Vec<_>>(),
        "class_annotation_warnings_total": class_warnings.len(),
    })
}

fn main() -> ExitCode {
    let Some(cli) = Cli::parse(std::env::args().skip(1)) else {
        eprintln!(
            "usage: horizon-native-java [--json] [--mixin-plans] [--weave <out.jar>] <mod.jar> \
             [main|client|server|neoforge] [class]"
        );
        return ExitCode::FAILURE;
    };
    let Cli {
        json,
        mixin_plans,
        weave,
        path,
        kind: requested_kind,
        class: explicit_class,
    } = cli;
    // Weaving is its own mode: it never executes the jar, so it needs neither
    // an entrypoint nor a loader kind.
    if let Some(output) = weave {
        return weave_main(&path, &output, json);
    }
    let mut kind = requested_kind.clone().unwrap_or_else(|| "main".to_owned());
    let jar = match NativeJar::from_path(&PathBuf::from(path)) {
        Ok(jar) => jar,
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"status": "error", "stage": "load", "error": error.to_string()})
                );
            } else {
                eprintln!("error loading jar: {error}");
            }
            return ExitCode::FAILURE;
        }
    };
    // Planning is a separate question from execution: report what the jar's
    // annotations would weave and stop, even when the jar declares no
    // entrypoint at all.
    if mixin_plans {
        println!("{}", mixin_plans_json(&jar));
        return ExitCode::SUCCESS;
    }
    let entrypoints = if let Some(class) = explicit_class {
        vec![horizon_native_java::Entrypoint {
            class,
            method: None,
            adapter: None,
        }]
    } else {
        let selected_kind = match requested_kind.as_deref() {
            Some(_) => Some(kind.clone()),
            None => ["main", "client", "server", "neoforge"]
                .into_iter()
                .find(|candidate| !jar.discover_entrypoints(candidate).is_empty())
                .map(str::to_owned),
        };
        if let Some(selected_kind) = selected_kind {
            kind = selected_kind.clone();
            jar.discover_entrypoints(&selected_kind)
        } else {
            Vec::new()
        }
    };
    if entrypoints.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::json!({"status": "error", "stage": "discover", "kind": kind, "error": "no entrypoint"})
            );
        } else {
            eprintln!("no {kind} entrypoint in metadata; pass its class as the third argument");
        }
        return ExitCode::FAILURE;
    };
    let mut host = jar.seed_host(StdHost::new());
    let mut calls = 0;
    let mut error = None;
    for entry in &entrypoints {
        if let Some(adapter) = entry.adapter.as_deref() {
            error = Some(format!(
                "{}: Fabric entrypoint adapter {adapter} is not supported by the native path",
                entry.class
            ));
            break;
        }
        match jar.execute(&entry.class, entry.method.as_deref(), &kind, &mut host) {
            Ok(report) => calls += report.calls,
            Err(entry_error) => {
                error = Some(format!("{}: {entry_error}", entry.class));
                break;
            }
        }
    }
    match error {
        None => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "ok",
                        "kind": kind,
                        "entrypoints": entrypoints.iter().map(|entry| &entry.class).collect::<Vec<_>>(),
                        "host_calls": calls,
                        "payload_directions": host
                            .payload_registrations()
                            .iter()
                            .map(|registration| registration.direction.clone())
                            .collect::<Vec<_>>(),
                        "registrations": host
                            .registrations()
                            .iter()
                            .map(|entry| serde_json::json!({
                                "registry": entry.registry,
                                "id": entry.id,
                                "typed": entry.declaration.is_some(),
                            }))
                            .collect::<Vec<_>>(),
                        "listeners": host.listener_registrations().count(),
                        "events": host.posted_events().len(),
                        "mixin_targets": mixin_target_labels(
                            jar.discovered_mixin_targets().iter()
                        ),
                        "mixin_plans": jar.mixin_injection_plans().len(),
                        "mixin_configs": jar.mixin_configs().len(),
                    })
                );
            } else {
                println!(
                    "native {} entrypoint(s) ran ({} host calls)",
                    entrypoints.len(),
                    calls
                );
            }
            ExitCode::SUCCESS
        }
        Some(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"status": "error", "stage": "execute", "error": error.to_string()})
                );
            } else {
                eprintln!("native execution stopped: {error}");
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClassFile, Cli, MethodRef, MixinInjectionPlan, Path, PathBuf, WeaveReport, read_jar,
        weave_jar, write_woven_jar,
    };
    use std::io::{Read, Write};

    fn parse(args: &[&str]) -> Option<Cli> {
        Cli::parse(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn json_flag_is_position_independent() {
        let expected = Cli {
            json: true,
            mixin_plans: false,
            weave: None,
            path: "mod.jar".into(),
            kind: Some("client".into()),
            class: Some("demo.Main".into()),
        };
        assert_eq!(
            parse(&["--json", "mod.jar", "client", "demo.Main"]),
            Some(expected.clone())
        );
        assert_eq!(
            parse(&["mod.jar", "--json", "client", "demo.Main"]),
            Some(expected)
        );
    }

    #[test]
    fn mixin_plan_flag_is_position_independent() {
        let expected = Cli {
            json: false,
            mixin_plans: true,
            weave: None,
            path: "mod.jar".into(),
            kind: None,
            class: None,
        };
        assert_eq!(parse(&["--mixin-plans", "mod.jar"]), Some(expected.clone()));
        assert_eq!(parse(&["mod.jar", "--mixin-plans"]), Some(expected));
    }

    #[test]
    fn weave_flag_takes_its_value_and_is_position_independent() {
        let expected = Cli {
            json: false,
            mixin_plans: false,
            weave: Some("out.jar".into()),
            path: "mod.jar".into(),
            kind: None,
            class: None,
        };
        assert_eq!(
            parse(&["--weave", "out.jar", "mod.jar"]),
            Some(expected.clone())
        );
        assert_eq!(parse(&["mod.jar", "--weave", "out.jar"]), Some(expected));
        // A value is required: `--weave` alone cannot fall back to a positional.
        assert_eq!(parse(&["mod.jar", "--weave"]), None);
    }

    #[test]
    fn bare_jar_is_a_valid_invocation() {
        assert_eq!(
            parse(&["mod.jar"]),
            Some(Cli {
                json: false,
                mixin_plans: false,
                weave: None,
                path: "mod.jar".into(),
                kind: None,
                class: None,
            })
        );
        assert_eq!(parse(&[]), None);
    }

    fn u2(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_be_bytes());
    }

    fn u4(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_be_bytes());
    }

    fn utf8(out: &mut Vec<u8>, value: &str) {
        out.push(1);
        u2(out, value.len() as u16);
        out.extend_from_slice(value.as_bytes());
    }

    /// A minimal class with one method `m`, the smallest shape the bounded
    /// weaver accepts: no branches, no exception table, no stack-map frames.
    /// Pool: 1 name, 2 Class 1, 3 `java/lang/Object`, 4 Class 3, 5 `m`,
    /// 6 descriptor, 7 `Code`.
    fn class_bytes(name: &str, access: u16, descriptor: &str, code: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        u4(&mut out, 0xCAFE_BABE);
        u2(&mut out, 0);
        u2(&mut out, 52);
        u2(&mut out, 8);
        utf8(&mut out, name);
        out.extend_from_slice(&[7, 0, 1]);
        utf8(&mut out, "java/lang/Object");
        out.extend_from_slice(&[7, 0, 3]);
        utf8(&mut out, "m");
        utf8(&mut out, descriptor);
        utf8(&mut out, "Code");
        u2(&mut out, 0x0021); // access
        u2(&mut out, 2); // this
        u2(&mut out, 4); // super
        u2(&mut out, 0); // interfaces
        u2(&mut out, 0); // fields
        u2(&mut out, 1); // methods
        u2(&mut out, access);
        u2(&mut out, 5);
        u2(&mut out, 6);
        u2(&mut out, 1); // method attributes
        u2(&mut out, 7);
        let mut body = Vec::new();
        u2(&mut body, 1); // max_stack
        u2(&mut body, 1); // max_locals
        u4(&mut body, code.len() as u32);
        body.extend_from_slice(code);
        u2(&mut body, 0); // exception table
        u2(&mut body, 0); // Code attributes
        u4(&mut out, body.len() as u32);
        out.extend_from_slice(&body);
        u2(&mut out, 0); // class attributes
        out
    }

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "horizon-native-java-weave-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
    }

    fn read_entry(archive: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Vec<u8> {
        let mut entry = archive.by_name(name).unwrap();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        data
    }

    #[test]
    fn weave_copies_resources_and_plan_free_classes_byte_identically() {
        let root = scratch("plain");
        let input = root.join("in.jar");
        let output = root.join("out.jar");
        let class = class_bytes("demo/Plain", 0x0008, "()V", &[0xb1]);
        let resource = b"# demo resource\n".to_vec();
        write_zip(
            &input,
            &[("demo/Plain.class", &class), ("assets/demo.txt", &resource)],
        );

        let report = weave_jar(&input, &output).expect("plan-free jar weaves");
        assert_eq!(
            report,
            WeaveReport {
                woven_classes: 0,
                untouched_classes: 1,
                woven_plans: 0,
                skipped_plans: 0,
                added_classes: 0,
                out: output.display().to_string(),
            }
        );
        assert_eq!(report.json()["status"], "error");
        let mut archive = zip::ZipArchive::new(std::fs::File::open(&output).unwrap()).unwrap();
        assert_eq!(read_entry(&mut archive, "assets/demo.txt"), resource);
        assert_eq!(read_entry(&mut archive, "demo/Plain.class"), class);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn weave_reports_a_rejected_plan_without_failing_the_jar() {
        let root = scratch("plans");
        let input = root.join("in.jar");
        let output = root.join("out.jar");
        let target_bytes = class_bytes("demo/T", 0x0008, "(I)V", &[0xb1]);
        let resource = b"{\"id\":\"demo\"}".to_vec();
        write_zip(
            &input,
            &[
                ("demo/T.class", &target_bytes),
                ("fabric.mod.json", &resource),
            ],
        );

        let (entries, classes) = read_jar(&input).unwrap();
        let target = MethodRef {
            owner: "demo/T".into(),
            name: "m".into(),
            descriptor: "(I)V".into(),
        };
        let callback = |name: &str| MethodRef {
            owner: "demo/Cb".into(),
            name: name.into(),
            descriptor: "(I)V".into(),
        };
        // HEAD applies; INVOKE needs the interpreter path, so the weaver must
        // report it as skipped instead of abandoning the whole class.
        let plans = [
            MixinInjectionPlan::inject_at_head(target.clone(), callback("before")),
            MixinInjectionPlan::inject_at_invoke(
                target,
                callback("invoke"),
                MethodRef {
                    owner: "java/lang/System".into(),
                    name: "gc".into(),
                    descriptor: "()V".into(),
                },
            ),
        ];

        let report = write_woven_jar(&output, &entries, &classes, &plans)
            .expect("a rejected plan still writes the jar");
        assert_eq!(report.woven_classes, 1);
        assert_eq!(report.untouched_classes, 0);
        assert_eq!(report.woven_plans, 1);
        assert_eq!(report.skipped_plans, 1);
        assert_eq!(report.status(), "ok");
        let mut archive = zip::ZipArchive::new(std::fs::File::open(&output).unwrap()).unwrap();
        assert_eq!(read_entry(&mut archive, "fabric.mod.json"), resource);
        let woven = read_entry(&mut archive, "demo/T.class");
        assert_ne!(woven, target_bytes, "the HEAD plan must rewrite the class");
        ClassFile::parse(&woven).expect("the woven class still parses");
        let _ = std::fs::remove_dir_all(root);
    }

    /// Executed rather than read back: `demo/Cb` is not in the jar, so each
    /// inserted call crosses the host boundary, which records the order the
    /// woven bytecode really runs them in.
    #[test]
    fn weave_runs_each_anchors_callbacks_in_the_order_given() {
        use horizon_native_java::NativeJar;

        let root = scratch("order");
        let input = root.join("in.jar");
        let output = root.join("out.jar");
        let target_bytes = class_bytes("demo/T", 0x0009, "()V", &[0xb1]);
        write_zip(&input, &[("demo/T.class", &target_bytes)]);

        let (entries, classes) = read_jar(&input).unwrap();
        let target = MethodRef {
            owner: "demo/T".into(),
            name: "m".into(),
            descriptor: "()V".into(),
        };
        let callback = |name: &str| MethodRef {
            owner: "demo/Cb".into(),
            name: name.into(),
            descriptor: "()V".into(),
        };
        // Neither anchor's names are alphabetical, and the anchors are mixed, so
        // reversing or sorting the whole list cannot pass by accident.
        let plans = [
            MixinInjectionPlan::inject_at_head(target.clone(), callback("zulu")),
            MixinInjectionPlan::inject_at_return(target.clone(), callback("yankee")),
            MixinInjectionPlan::inject_at_head(target.clone(), callback("alpha")),
            MixinInjectionPlan::inject_at_return(target, callback("bravo")),
        ];
        let report = write_woven_jar(&output, &entries, &classes, &plans).unwrap();
        assert_eq!(report.woven_plans, 4);
        assert_eq!(report.skipped_plans, 0);

        let jar = NativeJar::from_path(&output).unwrap();
        let mut host = horizon_native_java::StdHost::new();
        jar.execute("demo.T", Some("m"), "main", &mut host).unwrap();
        let ran = host
            .invocations()
            .iter()
            .filter(|invocation| invocation.method.owner == "demo/Cb")
            .map(|invocation| invocation.method.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ran, ["zulu", "alpha", "yankee", "bravo"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn mixin_plan_report_carries_configs_and_errors() {
        use super::mixin_plans_json;
        use horizon_native_java::NativeJar;
        use std::io::Write;

        let root = std::env::temp_dir().join(format!(
            "horizon-native-java-plan-report-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("report.jar");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("fabric.mod.json", options).unwrap();
        zip.write_all(br#"{"id":"report","mixins":["demo.mixins.json","broken.mixins.json"]}"#)
            .unwrap();
        zip.start_file("demo.mixins.json", options).unwrap();
        zip.write_all(
            br#"{"package":"demo.mixin","mixins":["DemoMixin"],"required":false,"priority":1500}"#,
        )
        .unwrap();
        zip.start_file("broken.mixins.json", options).unwrap();
        zip.write_all(br#"[]"#).unwrap();
        zip.finish().unwrap();

        let jar = NativeJar::from_path(&path).unwrap();
        let report = mixin_plans_json(&jar);
        assert_eq!(report["status"], "ok");
        assert!(report["plans"].as_array().unwrap().is_empty());
        let configs = report["configs"].as_array().unwrap();
        assert_eq!(configs.len(), 1, "unexpected configs: {configs:#?}");
        assert_eq!(configs[0]["path"], "demo.mixins.json");
        assert_eq!(configs[0]["package"], "demo.mixin");
        assert_eq!(
            configs[0]["mixins"],
            serde_json::json!(["demo.mixin.DemoMixin"])
        );
        assert_eq!(configs[0]["required"], false);
        assert_eq!(configs[0]["priority"], 1500);
        assert_eq!(configs[0]["mixin_priority"], serde_json::Value::Null);
        assert_eq!(
            report["class_annotation_warnings"],
            serde_json::json!([]),
            "a clean jar has no class-parse warnings"
        );
        assert_eq!(report["class_annotation_warnings_total"], 0);
        let errors = report["config_errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1, "unexpected errors: {errors:#?}");
        assert_eq!(errors[0][0], "broken.mixins.json");
        assert!(
            errors[0][1].as_str().unwrap().contains("JSON object"),
            "reason names the shape: {}",
            errors[0][1]
        );
        let _ = std::fs::remove_dir_all(root);
    }
}

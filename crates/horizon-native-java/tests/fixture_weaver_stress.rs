//! Stress fixture for combinations of every shape the bounded weaver supports.
//!
//! `combo` mixes a dense `tableswitch`, a sparse `lookupswitch`, a loop,
//! branches, a `try`/`catch` handler and eight `return` sites.  `wide` declares
//! 300 locals, so `javac` emits `wide` loads and the `RETURN` insert writes its
//! fresh local through a `wide` `astore`/`aload`.  Both methods carry a `HEAD`
//! and a `RETURN` plan.  Structural checks re-parse the woven class and prove
//! every branch, switch target, exception range and stack-map frame lands on an
//! instruction boundary; a fresh `URLClassLoader` over a jar checks sixteen
//! inputs against the original class, and the last test pins the rejections.

use horizon_native_java::{
    ClassFile, MethodRef, MixinInjectionPlan, ReturnMode, callback_info_returnable_class,
    weave_class, weave_class_with_extras, weave_return,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const CIR: &str = "Lorg/spongepowered/asm/mixin/injection/callback/CallbackInfoReturnable;";
const CIR_CLASS: &str = "org/spongepowered/asm/mixin/injection/callback/CallbackInfoReturnable";

const CALLBACK: &str = r#"package demo;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;
public final class Callback {
    private static int heads, returns;
    public static int heads() { return heads; }
    public static int returns() { return returns; }
    public static void comboHead(int a, int b) { heads++; }
    public static void comboReturn(int a, int b, CallbackInfoReturnable cir) { returns++; if (cir.getReturnValue() == null) throw new IllegalStateException("no result"); }
    public static void wideHead(int a) { heads++; }
    public static void wideReturn(int a, CallbackInfoReturnable cir) { returns++; if (a == 3) cir.setReturnValue(Integer.valueOf(4242)); }
    public static void touch(int x) { heads++; }
}"#;

const RUNNER: &str = r#"import java.lang.reflect.Method;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Paths;
public final class Runner {
    public static void main(String[] args) throws Exception {
        URL[] urls = { Paths.get(args[0]).toUri().toURL() };
        try (URLClassLoader loader = new URLClassLoader(urls, ClassLoader.getPlatformClassLoader())) {
            Class<?> target = Class.forName("demo.Target", true, loader);
            Class<?> callback = Class.forName("demo.Callback", true, loader);
            Method heads = callback.getMethod("heads"), returns = callback.getMethod("returns");
            Method combo = target.getMethod("combo", int.class, int.class);
            int[][] combos = { {0,7}, {1,7}, {2,3}, {3,9}, {4,1}, {5,100}, {6,50}, {5,7} };
            for (int[] input : combos) {
                Object result = combo.invoke(null, input[0], input[1]);
                System.out.println("combo " + input[0] + "," + input[1] + " " + result + " " + heads.invoke(null) + " " + returns.invoke(null));
            }
            Method wide = target.getMethod("wide", int.class);
            for (int a = 0; a <= 7; a++) {
                Object result = wide.invoke(null, a);
                System.out.println("wide " + a + " " + result + " " + heads.invoke(null) + " " + returns.invoke(null));
            }
        }
    }
}"#;

fn ref_to(owner: &str, name: &str, descriptor: &str) -> MethodRef {
    MethodRef {
        owner: owner.into(),
        name: name.into(),
        descriptor: descriptor.into(),
    }
}

fn method(name: &str, descriptor: &str) -> MethodRef {
    ref_to("demo/Target", name, descriptor)
}

fn callback(name: &str, descriptor: &str) -> MethodRef {
    ref_to("demo/Callback", name, descriptor)
}

fn target_source() -> String {
    let declarations = (0..300)
        .map(|index| format!("int v{index} = {index};"))
        .collect::<Vec<_>>()
        .join("\n        ");
    format!(
        "package demo;
public class Target {{
    public static int combo(int a, int b) {{
        int total = 0;
        for (int i = 0; i < a; i++) if ((i & 1) == 0) total += b; else total -= 1;
        switch (a) {{
            case 0: return total + 1;
            case 1: return total + 2;
            case 2: return total + 3;
            case 3: return total + 4;
            case 4: return total + 5;
            default: break;
        }}
        try {{ total += 100 / (a - 5); }} catch (ArithmeticException e) {{ total -= 7; }}
        switch (b) {{
            case 1: return total + 10;
            case 100: return total + 20;
            case 10000: return total + 30;
            default: return total;
        }}
    }}
    public static void touch(int x) {{ if (x > 0) return; }}
    public static int wide(int a) {{
        {declarations}
        int sum = 0;
        for (int i = 0; i < a; i++) if ((i & 1) == 0) sum += v255; else sum += v299;
        switch (a % 3) {{
            case 0: return sum + v0;
            case 1: return sum + v1;
            default: return sum + v2;
        }}
    }}
}}
"
    )
}

struct Jdk {
    javac: PathBuf,
    java: PathBuf,
    jar: PathBuf,
}

#[rustfmt::skip]
impl Jdk {
    fn find() -> Option<Self> {
        let tool = |name: &str| std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin").join(name)).unwrap_or_else(|| PathBuf::from(name));
        let jdk = Self { javac: tool("javac"), java: tool("java"), jar: tool("jar") };
        for tool in [&jdk.javac, &jdk.java] { Command::new(tool).arg("-version").output().ok()?; }
        Command::new(&jdk.jar).arg("--version").output().ok()?;
        Some(jdk)
    }
}

/// A temp directory removed when the test ends, even on panic.
struct Scratch(PathBuf);

#[rustfmt::skip]
impl Scratch {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("horizon-native-java-weaver-stress-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
}

#[rustfmt::skip]
impl Drop for Scratch {
    fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
}

fn compile(jdk: &Jdk, root: &Path) -> PathBuf {
    let src = root.join("src/demo");
    let classes = root.join("classes");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&classes).unwrap();
    let synthetic = classes.join(format!("{CIR_CLASS}.class"));
    fs::create_dir_all(synthetic.parent().unwrap()).unwrap();
    fs::write(&synthetic, callback_info_returnable_class().unwrap()).unwrap();
    fs::write(src.join("Target.java"), target_source()).unwrap();
    fs::write(src.join("Callback.java"), CALLBACK).unwrap();
    let output = Command::new(&jdk.javac)
        .args(["--release", "17", "-nowarn", "-cp"])
        .arg(&classes)
        .arg("-d")
        .arg(&classes)
        .arg(src.join("Target.java"))
        .arg(src.join("Callback.java"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "javac failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    classes
}

#[rustfmt::skip]
fn copy_classes(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let target = to.join(path.file_name().unwrap());
        if path.is_dir() { copy_classes(&path, &target); }
        else if path.extension().is_some_and(|extension| extension == "class") { fs::copy(&path, &target).unwrap(); }
    }
}

/// Replace `demo/Target` and add the synthetic classes in a copy of the compile
/// output, then package it as a jar for the fresh loader.
#[rustfmt::skip]
fn build_jar(jdk: &Jdk, classes: &Path, root: &Path, woven: &[u8], added: &BTreeMap<String, Vec<u8>>) -> PathBuf {
    let runner = root.join("runner");
    copy_classes(classes, &runner);
    fs::write(runner.join("demo/Target.class"), woven).unwrap();
    for (name, bytes) in added {
        let path = runner.join(format!("{name}.class"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
    }
    let jar = root.join("woven.jar");
    let output = Command::new(&jdk.jar).args(["--create", "--file"]).arg(&jar).args(["-C"]).arg(&runner).arg(".").output().unwrap();
    assert!(output.status.success(), "jar failed: {}", String::from_utf8_lossy(&output.stderr));
    jar
}

#[rustfmt::skip]
fn run(jdk: &Jdk, root: &Path, location: &Path) -> Vec<String> {
    let source = root.join("Runner.java");
    fs::write(&source, RUNNER).unwrap();
    let output = Command::new(&jdk.java).arg(&source).arg(location).output().unwrap();
    assert!(output.status.success(), "the JVM rejected the woven classes: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).lines().filter(|line| line.starts_with("combo ") || line.starts_with("wide ")).map(str::to_owned).collect()
}

/// `(key, result, heads, returns)` per invocation.
#[rustfmt::skip]
fn parse(lines: &[String]) -> Vec<(String, String, u32, u32)> {
    lines.iter().map(|line| {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        assert_eq!(fields.len(), 5, "unexpected runner line: {line}");
        (format!("{}:{}", fields[0], fields[1]), fields[2].to_owned(), fields[3].parse().unwrap(), fields[4].parse().unwrap())
    }).collect()
}

#[rustfmt::skip]
struct R<'a> { b: &'a [u8], at: usize }

#[rustfmt::skip]
impl<'a> R<'a> {
    fn new(b: &'a [u8]) -> Self { Self { b, at: 0 } }
    fn u1(&mut self) -> u8 { let v = self.b[self.at]; self.at += 1; v }
    fn u2(&mut self) -> u16 { let v = u16::from_be_bytes([self.b[self.at], self.b[self.at + 1]]); self.at += 2; v }
    fn u4(&mut self) -> u32 { let v = u32::from_be_bytes([self.b[self.at], self.b[self.at + 1], self.b[self.at + 2], self.b[self.at + 3]]); self.at += 4; v }
    fn skip(&mut self, n: usize) { self.at += n; }
    fn bytes(&mut self, n: usize) -> &'a [u8] { let s = &self.b[self.at..self.at + n]; self.at += n; s }
}

#[rustfmt::skip]
fn read_pool(r: &mut R) -> Vec<Option<String>> {
    let count = r.u2() as usize;
    let mut pool = vec![None; count.max(1)];
    let mut index = 1;
    while index < count {
        let tag = r.u1();
        match tag {
            1 => { let n = r.u2() as usize; pool[index] = Some(String::from_utf8_lossy(r.bytes(n)).into_owned()); }
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => r.skip(4),
            5 | 6 => { r.skip(8); index += 1; }
            7 | 8 | 16 | 19 | 20 => r.skip(2),
            15 => r.skip(3),
            other => panic!("unexpected constant pool tag {other}"),
        }
        index += 1;
    }
    pool
}

#[rustfmt::skip]
fn skip_attributes(r: &mut R) {
    for _ in 0..r.u2() { let _ = r.u2(); let length = r.u4() as usize; r.skip(length); }
}

#[rustfmt::skip]
fn skip_verification(r: &mut R) {
    match r.u1() { 0..=6 => {} 7 | 8 => r.skip(2), other => panic!("reserved verification type {other}") }
}

#[rustfmt::skip]
fn parse_frames(body: &[u8]) -> Vec<u32> {
    let mut r = R::new(body);
    let count = r.u2() as usize;
    let mut offsets = Vec::with_capacity(count);
    for _ in 0..count {
        let frame_type = r.u1();
        let delta = match frame_type {
            0..=63 => frame_type as u32,
            64..=127 => { skip_verification(&mut r); (frame_type - 64) as u32 }
            247 => { let delta = r.u2() as u32; skip_verification(&mut r); delta }
            248..=251 => r.u2() as u32,
            252..=254 => { let delta = r.u2() as u32; for _ in 0..frame_type - 251 { skip_verification(&mut r); } delta }
            255 => {
                let delta = r.u2() as u32;
                for _ in 0..r.u2() { skip_verification(&mut r); }
                for _ in 0..r.u2() { skip_verification(&mut r); }
                delta
            }
            other => panic!("reserved stack-map frame type {other}"),
        };
        offsets.push(offsets.last().map_or(delta, |previous| previous + delta + 1));
    }
    offsets
}

struct CodeView {
    code: Vec<u8>,
    max_stack: u16,
    max_locals: u16,
    /// `(start_pc, end_pc, handler_pc)` per exception-table entry.
    handlers: Vec<(u16, u16, u16)>,
    frames: Vec<u32>,
}

#[rustfmt::skip]
fn parse_code_body(pool: &[Option<String>], body: &[u8]) -> CodeView {
    let mut r = R::new(body);
    let max_stack = r.u2();
    let max_locals = r.u2();
    let length = r.u4() as usize;
    let code = r.bytes(length).to_vec();
    let mut handlers = Vec::new();
    for _ in 0..r.u2() {
        let start = r.u2(); let end = r.u2(); let handler = r.u2(); let _ = r.u2();
        handlers.push((start, end, handler));
    }
    let mut frames = Vec::new();
    for _ in 0..r.u2() {
        let name = r.u2() as usize;
        let length = r.u4() as usize;
        let body = r.bytes(length);
        if pool.get(name).and_then(Option::as_deref) == Some("StackMapTable") { frames = parse_frames(body); }
    }
    CodeView { code, max_stack, max_locals, handlers, frames }
}

#[rustfmt::skip]
fn code_view(class: &[u8], name: &str, descriptor: &str) -> CodeView {
    assert_eq!(&class[..4], &[0xCA, 0xFE, 0xBA, 0xBE], "bad class magic");
    let mut r = R::new(class);
    r.skip(8);
    let pool = read_pool(&mut r);
    r.skip(6); // access_flags, this_class, super_class
    let interfaces = r.u2() as usize;
    r.skip(interfaces * 2);
    for _ in 0..r.u2() { r.skip(6); skip_attributes(&mut r); }
    for _ in 0..r.u2() {
        let _ = r.u2();
        let name_index = r.u2() as usize;
        let descriptor_index = r.u2() as usize;
        for _ in 0..r.u2() {
            let attribute = r.u2() as usize;
            let length = r.u4() as usize;
            let body = r.bytes(length);
            let text = |index: usize| pool.get(index).and_then(Option::as_deref);
            if text(name_index) == Some(name) && text(descriptor_index) == Some(descriptor) && text(attribute) == Some("Code") {
                return parse_code_body(&pool, body);
            }
        }
    }
    panic!("class has no Code for {name}{descriptor}");
}

/// The alignment padding a switch at `at` needs: its operands start on a
/// 4-byte boundary counted from the start of `code`.
fn switch_padding(at: usize) -> usize {
    (4 - (at + 1) % 4) % 4
}

struct Walk {
    starts: BTreeSet<usize>,
    /// `(site, target)` per branch and switch target.
    targets: Vec<(usize, usize)>,
    /// `(offset, padding, targets)` per switch.
    switches: Vec<(usize, usize, Vec<usize>)>,
}

fn resolve(code: &[u8], at: usize, delta: i64) -> usize {
    let target = at as i64 + delta;
    assert!(
        target >= 0 && (target as usize) < code.len(),
        "the instruction at {at} targets {target}, outside the {}-byte code",
        code.len()
    );
    target as usize
}

fn read_i32(code: &[u8], at: usize) -> i32 {
    i32::from_be_bytes([code[at], code[at + 1], code[at + 2], code[at + 3]])
}

#[rustfmt::skip]
fn fixed_size(opcode: u8) -> usize {
    match opcode {
        0x00..=0x0f | 0x1a..=0x35 | 0x3b..=0x83 | 0x85..=0x98 | 0xac..=0xb1 | 0xbe..=0xbf | 0xc2..=0xc3 => 1,
        0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3a | 0xa9 | 0xbc => 2,
        0x11 | 0x13 | 0x14 | 0x84 | 0x99..=0xa8 | 0xb2..=0xb8 | 0xbb | 0xbd | 0xc0 | 0xc1 | 0xc6..=0xc9 => 3,
        0xc5 => 4,
        0xb9 | 0xba => 5,
        other => panic!("unexpected opcode 0x{other:02x}"),
    }
}

#[rustfmt::skip]
fn walk(code: &[u8]) -> Walk {
    let mut walk = Walk { starts: BTreeSet::new(), targets: Vec::new(), switches: Vec::new() };
    let mut at = 0usize;
    while at < code.len() {
        walk.starts.insert(at);
        let opcode = code[at];
        match opcode {
            0xaa => {
                let header = at + 1 + switch_padding(at);
                assert_eq!(header % 4, 0, "tableswitch at {at} is misaligned");
                let default = read_i32(code, header);
                let (low, high) = (read_i32(code, header + 4), read_i32(code, header + 8));
                let mut field = header + 12;
                let mut targets = vec![resolve(code, at, i64::from(default))];
                for _ in low..=high { targets.push(resolve(code, at, i64::from(read_i32(code, field)))); field += 4; }
                walk.targets.extend(targets.iter().map(|target| (at, *target)));
                walk.switches.push((at, switch_padding(at), targets));
                at = field;
            }
            0xab => {
                let header = at + 1 + switch_padding(at);
                assert_eq!(header % 4, 0, "lookupswitch at {at} is misaligned");
                let default = read_i32(code, header);
                let pairs = read_i32(code, header + 4);
                assert!(pairs >= 0, "lookupswitch at {at} has {pairs} pairs");
                let mut field = header + 8;
                let mut targets = vec![resolve(code, at, i64::from(default))];
                for _ in 0..pairs { field += 4; targets.push(resolve(code, at, i64::from(read_i32(code, field)))); field += 4; }
                walk.targets.extend(targets.iter().map(|target| (at, *target)));
                walk.switches.push((at, switch_padding(at), targets));
                at = field;
            }
            0xc4 => {
                at += match code[at + 1] {
                    0x15..=0x19 | 0x36..=0x3a | 0xa9 => 4,
                    0x84 => 6,
                    other => panic!("wide prefixes opcode 0x{other:02x} at {at}"),
                };
            }
            0x99..=0xa8 | 0xc6 | 0xc7 => {
                let delta = i16::from_be_bytes([code[at + 1], code[at + 2]]);
                walk.targets.push((at, resolve(code, at, i64::from(delta))));
                at += 3;
            }
            0xc8 | 0xc9 => {
                walk.targets.push((at, resolve(code, at, i64::from(read_i32(code, at + 1)))));
                at += 5;
            }
            _ => at += fixed_size(opcode),
        }
    }
    walk
}

#[rustfmt::skip]
fn invokestatic_sites(view: &CodeView) -> usize {
    let walk = walk(&view.code);
    walk.starts.iter().filter(|start| view.code[**start] == 0xb8).count()
}

#[rustfmt::skip]
fn return_sites(view: &CodeView) -> usize {
    let walk = walk(&view.code);
    walk.starts.iter().filter(|start| matches!(view.code[**start], 0xac..=0xb1)).count()
}

/// A non-void RETURN insert ends `... aload <fresh>; invokestatic callback;
/// aload <fresh>; invokevirtual getReturnValue ...`, so the callback call is the
/// only `invokestatic` followed by an `aload` and a virtual call.
#[rustfmt::skip]
fn return_callback_sites(view: &CodeView) -> usize {
    let walk = walk(&view.code);
    walk.starts.iter().filter(|start| {
        view.code[**start] == 0xb8 && matches!(view.code[**start + 3], 0x19 | 0xc4) && {
            let after_aload = if view.code[**start + 3] == 0xc4 { **start + 7 } else { **start + 5 };
            view.code.get(after_aload) == Some(&0xb6)
        }
    }).count()
}

/// Every branch, switch target, exception range and frame names an instruction
/// boundary; each switch re-derives its padding; the byte delta is all inserts.
#[rustfmt::skip]
fn assert_relocated(source: &CodeView, woven: &CodeView, label: &str) {
    let before = walk(&source.code);
    let after = walk(&woven.code);
    assert_eq!(after.targets.len(), before.targets.len(), "{label}: branch target count");
    assert_eq!(after.switches.len(), before.switches.len(), "{label}: switch count");
    for (site, target) in &after.targets {
        assert!(after.starts.contains(target), "{label}: {site} targets {target}, not an instruction start");
    }
    for (site, padding, _) in &after.switches {
        assert_eq!(*padding, switch_padding(*site), "{label}: stale switch padding");
    }
    assert_eq!(woven.handlers.len(), source.handlers.len(), "{label}: handler count");
    for (start, end, handler) in &woven.handlers {
        assert!(*start < *end && *end as usize <= woven.code.len(), "{label}: bad exception range {start}..{end}");
        assert!(after.starts.contains(&(*start as usize)), "{label}: range start {start} is not an instruction start");
        assert!(*end as usize == woven.code.len() || after.starts.contains(&(*end as usize)), "{label}: range end {end} is not an instruction start");
        assert!(after.starts.contains(&(*handler as usize)), "{label}: handler {handler} is not an instruction start");
    }
    assert!(woven.frames.len() >= source.frames.len() && woven.frames.windows(2).all(|pair| pair[0] < pair[1]), "{label}: a frame was dropped or reordered");
    for frame in &woven.frames {
        assert!(after.starts.contains(&(*frame as usize)), "{label}: frame at {frame} is not an instruction start");
    }
    let padding = |walk: &Walk| walk.switches.iter().map(|(_, pad, _)| *pad as i64).sum::<i64>();
    let inserted = (woven.code.len() as i64 - source.code.len() as i64) - (padding(&after) - padding(&before));
    assert!(inserted > 0, "{label}: nothing was inserted");
}

#[rustfmt::skip]
fn check_method(original: &[u8], woven: &[u8], name: &str, descriptor: &str) {
    let before = code_view(original, name, descriptor);
    let after = code_view(woven, name, descriptor);
    assert_relocated(&before, &after, name);
    assert!(after.max_stack > before.max_stack, "{name}: max_stack was not raised");
    assert!(after.max_locals > before.max_locals, "{name}: the RETURN insert did not claim a fresh local");
    let sites = return_sites(&before);
    assert!(sites >= 2, "{name}: expected several return sites, got {sites}");
    // The fixture's own code calls nothing static, so the inserts account for
    // every `invokestatic`: one HEAD callback, and `valueOf` plus the callback
    // at each return site.
    assert_eq!(invokestatic_sites(&after), 1 + 2 * sites, "{name}: one callback per HEAD and RETURN site");
    assert_eq!(return_callback_sites(&after), sites, "{name}: one RETURN callback per return site");
}

#[test]
#[rustfmt::skip]
fn combined_features_weave_and_run() {
    let Some(jdk) = Jdk::find() else { eprintln!("skipping: no JDK (javac, java and jar) on this machine"); return; };
    let scratch = Scratch::new("combined");
    let classes = compile(&jdk, &scratch.0);
    let bytes = fs::read(classes.join("demo/Target.class")).unwrap();
    let class = ClassFile::parse(&bytes).unwrap();
    let (ret_combo, ret_wide) = (format!("(II{CIR})V"), format!("(I{CIR})V"));
    let plans = [
        MixinInjectionPlan::inject_at_head(method("combo", "(II)I"), callback("comboHead", "(II)V")),
        MixinInjectionPlan::inject_at_return(method("combo", "(II)I"), callback("comboReturn", &ret_combo)),
        MixinInjectionPlan::inject_at_head(method("wide", "(I)I"), callback("wideHead", "(I)V")),
        MixinInjectionPlan::inject_at_return(method("wide", "(I)I"), callback("wideReturn", &ret_wide)),
    ];
    let (woven, added) = weave_class_with_extras(&class, &plans).unwrap();
    assert_eq!(weave_class(&class, &plans).unwrap(), woven, "weave_class and weave_class_with_extras must agree");
    assert!(added.contains_key(CIR_CLASS), "a non-void RETURN insert must report the synthetic class");
    assert_eq!(ClassFile::parse(&woven).unwrap().write().unwrap(), woven, "the woven class round-trips");

    // The fixture really contains both switch encodings, a handler and wide
    // locals before weaving; otherwise the combinations prove nothing.
    let combo = code_view(&bytes, "combo", "(II)I");
    let combo_walk = walk(&combo.code);
    assert_eq!(combo_walk.switches.len(), 2, "combo has two switches");
    let opcodes = combo_walk.switches.iter().map(|(site, _, _)| combo.code[*site]).collect::<Vec<_>>();
    assert!(opcodes.contains(&0xaa) && opcodes.contains(&0xab), "combo must combine a dense and a sparse switch, got {opcodes:02x?}");
    assert!(!combo.handlers.is_empty(), "combo has a try/catch handler");
    assert!(code_view(&bytes, "wide", "(I)I").code.contains(&0xc4), "wide must use a wide-prefixed local instruction");
    check_method(&bytes, &woven, "combo", "(II)I");
    check_method(&bytes, &woven, "wide", "(I)I");

    // The original classes give the expected values; the woven jar runs in a
    // fresh loader whose parent cannot see `demo`.
    let baseline = parse(&run(&jdk, &scratch.0, &classes));
    let jar = build_jar(&jdk, &classes, &scratch.0, &woven, &added);
    let actual = parse(&run(&jdk, &scratch.0, &jar));
    assert_eq!(baseline.len(), 16, "eight combo and eight wide inputs");
    assert_eq!(actual.len(), baseline.len());
    for (index, (expected, measured)) in baseline.iter().zip(&actual).enumerate() {
        assert_eq!(expected.0, measured.0, "invocation order");
        assert_eq!(measured.2, index as u32 + 1, "{}: HEAD callback count", measured.0);
        assert_eq!(measured.3, index as u32 + 1, "{}: RETURN callback count", measured.0);
        // `wide(3)` proves the RETURN callback's override; every other input
        // must equal the original class, so callbacks changed nothing else.
        let want = if measured.0 == "wide:3" { "4242" } else { expected.1.as_str() };
        assert_eq!(measured.1, want, "{}: wrong result", measured.0);
    }
}

#[test]
#[rustfmt::skip]
fn rejected_boundaries_are_documented() {
    let Some(jdk) = Jdk::find() else { eprintln!("skipping: no JDK (javac, java and jar) on this machine"); return; };
    let scratch = Scratch::new("rejected");
    let classes = compile(&jdk, &scratch.0);
    let bytes = fs::read(classes.join("demo/Target.class")).unwrap();
    let class = ClassFile::parse(&bytes).unwrap();

    // Cancelling a void target needs a runtime-owned CallbackInfo.
    let void_plan = MixinInjectionPlan { target: method("touch", "(I)V"), callback: callback("touch", "(I)V"), at: "RETURN".into(), cancellable: true, invoke_target: None, ordinal: None, slice: None, shift: None };
    let error = weave_class(&class, &[void_plan]).unwrap_err();
    assert_eq!(error, "demo/Target.touch: cancellable RETURN weaving needs a runtime-owned CallbackInfo, which the bounded weaver cannot construct");

    // A condy constant pool is outside the writer.
    let condy = ClassFile::parse(&add_condy(bytes.clone())).unwrap();
    let head_plan = MixinInjectionPlan::inject_at_head(method("combo", "(II)I"), callback("comboHead", "(II)V"));
    let error = weave_class(&condy, std::slice::from_ref(&head_plan)).unwrap_err();
    assert_eq!(error, "demo/Target.combo: class constant pool uses CONSTANT_Dynamic (condy), which the bounded weaver does not rewrite");

    // `@At("INVOKE")` now goes through the selected call-site entry point.
    // `combo` declares no `Math.abs` call, so the plan is a precise reason
    // instead of the old blanket anchor rejection.
    let invoke_plan = MixinInjectionPlan::inject_at_invoke(method("combo", "(II)I"), callback("comboHead", "(II)V"), ref_to("java/lang/Math", "abs", "(I)I"));
    let error = weave_class(&class, &[invoke_plan]).unwrap_err();
    assert!(error.contains("INVOKE weaving found no"), "{error}");
    assert!(error.contains("interpreter"), "{error}");

    // TAIL on a void target stays supported: one callback before only the last
    // return, so direct `weave_return` covers the public single-anchor API.
    let before = code_view(&bytes, "touch", "(I)V");
    assert_eq!(return_sites(&before), 2, "touch has two return sites");
    let direct = weave_return(&class, &method("touch", "(I)V"), &callback("touch", "(I)V"), ReturnMode::Tail).unwrap();
    let after = code_view(&direct, "touch", "(I)V");
    assert_eq!(return_sites(&after), 2);
    assert_eq!(invokestatic_sites(&after), 1);
}

/// Append one unused `CONSTANT_Dynamic` entry to a class's pool: `javac` never
/// emits condy, and the rejection is decided by the tag.
#[rustfmt::skip]
fn add_condy(bytes: Vec<u8>) -> Vec<u8> {
    let mut at = 10usize;
    let mut pool = bytes;
    let count = u16::from_be_bytes([pool[8], pool[9]]) as usize;
    let mut index = 1;
    while index < count {
        let tag = pool[at];
        at += 1;
        at += match tag {
            1 => 2 + u16::from_be_bytes([pool[at], pool[at + 1]]) as usize,
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => 4,
            5 | 6 => { index += 1; 8 }
            7 | 8 | 16 | 19 | 20 => 2,
            15 => 3,
            other => panic!("unexpected constant pool tag {other}"),
        };
        index += 1;
    }
    pool.splice(at..at, [17u8, 0, 0, 0, 0]);
    pool[8..10].copy_from_slice(&((count + 1) as u16).to_be_bytes());
    pool
}

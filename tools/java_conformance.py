#!/usr/bin/env python3
"""Differential conformance harness for horizon-native-java (docs/java-interpreter.md).

HotSpot is the oracle. Every program in tools/java_conformance.json is compiled with javac and run
three ways: on HotSpot (`java`), on the interpreter natively (`horizon-java-run`) and on the
interpreter's wasm32-wasip1 build under Node's WASI (tools/run_wasi.mjs). A run passes when its
stdout, exit code and uncaught exception class match HotSpot's; the native and wasm runs must also
agree with each other. Programs that need files, the network, JNI, processes or JDK internals are
run but left out of the pass rate (`needs` in the manifest), and so is a program whose HotSpot
output differs between two runs (it prints a hash, a time or a thread race).

    python3 tools/java_conformance.py                     # the program corpus, every engine
    python3 tools/java_conformance.py --filter lambda     # programs whose id matches a regex
    python3 tools/java_conformance.py --engines hotspot,native
    python3 tools/java_conformance.py --mods              # mod-shaped jars: what reaches the Host
    python3 tools/java_conformance.py --real-mods DIR     # local jars: entrypoints and typed registrations
    python3 tools/java_conformance.py --bench             # CPU-bound programs, timed
    python3 tools/java_conformance.py --bench --rusty-jvm PATH/rusty-jvm

The corpus lives in mods/horizon/src/conformance/java/dev/horizonlab/conformance/: `rustyjvm/` is
rusty-jvm's test programs (MIT, its LICENSE beside them), `lang/` Horizon's own, `mods/` the
mod-shaped jars and the stub Minecraft and loader classes they compile against (the stubs never go
into a jar), `bench/` the benchmarks. A few programs are assembled here instead, for opcodes javac
never emits. Needs a JDK 25 (`--java-home`, `JAVA_HOME`, or /usr/lib/jvm/java-25-*), Node 22 for
the wasm runs, and the wasm32-wasip1 Rust target.
"""
import argparse
import concurrent.futures
import json
import os
import pathlib
import re
import shutil
import statistics
import struct
import subprocess
import sys
import time
import zipfile

try:  # imported as tools.java_conformance (tests/), or run as a script
    from . import lab
except ImportError:
    import lab

ROOT = pathlib.Path(__file__).resolve().parent.parent

CORPUS = ROOT / "mods/horizon/src/conformance/java/dev/horizonlab/conformance"
MANIFEST = ROOT / "tools/java_conformance.json"
WASI = ROOT / "tools/run_wasi.mjs"
RUNNER = "horizon-java-run"
# What a program may need that the browser's sandbox does not give a mod. These run, but stay out
# of the pass rate.
EXCLUDED_NEEDS = {"files", "network", "jni", "process", "jdk-internal", "env", "launcher"}
# The runner's exit code when the interpreter itself stopped (an unmodelled opcode or call).
INTERPRETER_ERROR = 70
EXCEPTION_LINE = re.compile(r'^Exception in thread "[^"]*" ([\w.$]+)(?::\s?(.*))?$')
CAUSED_BY = re.compile(r"^Caused by: ([\w.$]+)(?::\s?(.*))?$")


# ---------------------------------------------------------------------------- toolchain


def java_home(arg):
    """A JDK 25 or newer: --java-home, JAVA_HOME, or the system's java-25."""
    system = pathlib.Path("/usr/lib/jvm")
    candidates = [arg, os.environ.get("JAVA_HOME")]
    if system.is_dir():
        candidates += sorted(str(p) for p in system.glob("java-2[5-9]*"))
    for home in filter(None, candidates):
        java = pathlib.Path(home) / "bin" / "java"
        if not java.exists():
            continue
        version = subprocess.run([str(java), "-version"], capture_output=True, text=True, env=java_env())
        match = re.search(r'version "(\d+)', version.stderr)
        if match and int(match.group(1)) >= 25:
            return pathlib.Path(home)
    sys.exit("java_conformance: needs a JDK 25 or newer (--java-home or JAVA_HOME)")


def java_env():
    """The environment for java and javac: no option variables, whose notices would land on stderr,
    and a UTF-8 C locale, so HotSpot's default locale (`en`) and stdout encoding are the
    interpreter's whatever the machine's are."""
    env = dict(os.environ)
    for name in ("JAVA_TOOL_OPTIONS", "_JAVA_OPTIONS", "JDK_JAVA_OPTIONS"):
        env.pop(name, None)
    env["LC_ALL"] = env["LANG"] = "C.UTF-8"
    return env


def build_runners(engines, skip):
    """Builds the runner natively and for wasm32-wasip1; returns their paths."""
    target = lab.cargo_target()
    native = target / "release" / RUNNER
    wasm = target / "wasm32-wasip1" / "release" / f"{RUNNER}.wasm"
    toolchain = ["+nightly"] if shutil.which("rustup") and "nightly" in subprocess.run(
        ["rustup", "toolchain", "list"], capture_output=True, text=True).stdout else []
    base = ["build", "--release", "-p", "horizon-native-java", "--bin", RUNNER]
    if not skip:
        if "native" in engines:
            subprocess.run(["cargo", *base], cwd=ROOT, check=True)
        if "wasm" in engines:
            subprocess.run(["cargo", *toolchain, *base, "--target", "wasm32-wasip1"], cwd=ROOT, check=True)
    return native, wasm


# ---------------------------------------------------------------------------- the corpus


def load_manifest():
    return json.loads(MANIFEST.read_text())


def compile_suite(home, suite, spec, out):
    """javac over one suite's sources into out/<suite>/classes; returns that folder."""
    classes = out / suite / "classes"
    if classes.exists():
        shutil.rmtree(classes)
    classes.mkdir(parents=True)
    if spec.get("generated"):
        assemble_generated(classes)
        return classes
    src = CORPUS / spec["dir"]
    special = spec.get("javac", {})
    skipped = set(spec.get("skip", []))
    files = sorted(p for p in src.rglob("*.java")
                   if p.name not in special and p.name not in skipped
                   and not any(part in spec.get("exclude_dirs", []) for part in p.relative_to(src).parts))
    javac = [str(home / "bin" / "javac"), "-nowarn", "-encoding", "UTF-8", "-d", str(classes)]
    run = subprocess.run([*javac, *map(str, files)], capture_output=True, text=True, env=java_env())
    if run.returncode != 0:
        sys.exit(f"java_conformance: javac failed for {suite}:\n{run.stderr[-4000:]}")
    for name, flags in sorted(special.items()):
        run = subprocess.run([*javac, "-cp", str(classes), *flags, str(src / name)],
                             capture_output=True, text=True, env=java_env())
        if run.returncode != 0:
            print(f"java_conformance: {name} did not compile ({' '.join(flags)}): {run.stderr.strip()[:300]}",
                  file=sys.stderr)
    return classes


def make_jar(classes, jar, include=None, resources=None):
    """A deterministic jar of the class files (those `include` accepts) and extra resources."""
    with zipfile.ZipFile(jar, "w", zipfile.ZIP_DEFLATED) as out:
        for path in sorted(classes.rglob("*.class")):
            name = path.relative_to(classes).as_posix()
            if include and not include(name):
                continue
            info = zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            out.writestr(info, path.read_bytes())
        for name, data in sorted((resources or {}).items()):
            info = zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            out.writestr(info, data)
    return jar


# Programs javac never compiles to, assembled here: rusty-jvm keeps them as class files built
# with an assembler. Each prints System.out.println(<one static call>).
GENERATED = {
    # name: (static method descriptor, argument constants, code, max_stack, max_locals)
    "samples/opcodes/nop/NopGeneratedExample": (
        "()I", [], [("ldc_i", 5), ("ldc_i", 7), 0x00, 0x00, 0x00, 0x00, 0x60, 0xAC], 2, 0),
    "samples/opcodes/swap/SwapGeneratedExample": (
        "()I", [], [("ldc_i", 5), ("ldc_i", 7), 0x5F, 0x64, 0xAC], 2, 0),
    "samples/opcodes/pop2/Pop2GeneratedExample": (
        "()I", [], [("ldc_i", 5), ("ldc_i", 7), 0x58, ("ldc_i", 1000), 0xAC], 2, 0),
    "samples/opcodes/dup_x2/DupX2GeneratedExample": (
        "(FFF)F", [2.0, 30.0, 400.0], [0x22, 0x23, 0x24, 0x5B, 0x62, 0x6A, 0x62, 0xAE], 4, 3),
    "samples/opcodes/dup2_x1/Dup2_X1GeneratedExample": (
        "()I", [], [0x06, 0x05, 0x04, 0x5D, 0x3B, 0x3C, 0x3D, 0x3E, 0x36, 4,
                    0x15, 4, 0x11, 0x27, 0x10, 0x68, 0x1D, 0x11, 0x03, 0xE8, 0x68, 0x60,
                    0x1C, 0x10, 100, 0x68, 0x60, 0x1B, 0x10, 10, 0x68, 0x60, 0x1A, 0x60, 0xAC], 5, 5),
    "samples/opcodes/dup2_x2/Dup2_X2GeneratedExample": (
        "()I", [], [0x07, 0x06, 0x05, 0x04, 0x5E, 0x3B, 0x3C, 0x3D, 0x3E, 0x36, 4, 0x36, 5,
                    0x15, 5, ("ldc_i", 100000), 0x68, 0x15, 4, 0x11, 0x27, 0x10, 0x68, 0x60,
                    0x1D, 0x11, 0x03, 0xE8, 0x68, 0x60, 0x1C, 0x10, 100, 0x68, 0x60,
                    0x1B, 0x10, 10, 0x68, 0x60, 0x1A, 0x60, 0xAC], 6, 6),
    # max_stack 1 under three pushes: HotSpot's verifier refuses the class.
    "samples/invalidprograms/operandstackoverflow/OperandStackOverflowExample": (
        "()I", [], [("ldc_i", 10), ("ldc_i", 20), ("ldc_i", 30), 0xAC], 1, 0),
}


def assemble_generated(classes):
    for name, (desc, constants, code, max_stack, max_locals) in GENERATED.items():
        path = classes / f"{name}.class"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(assemble(name, desc, constants, code, max_stack, max_locals))


def assemble(name, desc, constants, code, max_stack, max_locals):
    """One class: `static <desc> run(...)` with `code`, and a main printing its result."""
    pool = []
    index = {}

    def const(kind, *value):
        key = (kind, value)
        if key not in index:
            pool.append(key)
            index[key] = len(pool)  # every entry here takes one slot
        return index[key]

    def utf8(text):
        return const("utf8", text)

    def cls(text):
        return const("class", utf8(text))

    def nat(n, d):
        return const("nat", utf8(n), utf8(d))

    this = cls(name)
    sup = cls("java/lang/Object")
    ret = desc[desc.index(")") + 1:]
    run_ref = const("method", this, nat("run", desc))
    out_ref = const("field", cls("java/lang/System"), nat("out", "Ljava/io/PrintStream;"))
    println = const("method", cls("java/io/PrintStream"), nat("println", f"({ret})V"))

    body = bytearray()
    for op in code:
        if isinstance(op, tuple):
            body += bytes([0x13]) + struct.pack(">H", const("int", op[1]))
        else:
            body.append(op)
    main = bytearray([0xB2]) + struct.pack(">H", out_ref)
    for value in constants:
        main += bytes([0x13]) + struct.pack(">H", const("float", value))
    main += bytes([0xB8]) + struct.pack(">H", run_ref)
    main += bytes([0xB6]) + struct.pack(">H", println) + bytes([0xB1])
    code_name = utf8("Code")
    methods = [
        (0x0009, utf8("main"), utf8("([Ljava/lang/String;)V"), 2 + len(constants), 1, bytes(main)),
        (0x000A, utf8("run"), utf8(desc), max_stack, max_locals, bytes(body)),
    ]
    out = bytearray(struct.pack(">IHH", 0xCAFEBABE, 0, 52))
    entries = bytearray()
    for kind, value in pool:
        if kind == "utf8":
            data = value[0].encode()
            entries += bytes([1]) + struct.pack(">H", len(data)) + data
        elif kind == "int":
            entries += bytes([3]) + struct.pack(">i", value[0])
        elif kind == "float":
            entries += bytes([4]) + struct.pack(">f", value[0])
        elif kind == "class":
            entries += bytes([7]) + struct.pack(">H", value[0])
        elif kind == "field":
            entries += bytes([9]) + struct.pack(">HH", *value)
        elif kind == "method":
            entries += bytes([10]) + struct.pack(">HH", *value)
        elif kind == "nat":
            entries += bytes([12]) + struct.pack(">HH", *value)
    out += struct.pack(">H", len(pool) + 1) + entries
    out += struct.pack(">HHHHH", 0x0021, this, sup, 0, 0)
    out += struct.pack(">H", len(methods))
    for flags, mname, mdesc, stack, locals_, mcode in methods:
        attribute = struct.pack(">HHI", stack, locals_, len(mcode)) + mcode + struct.pack(">HH", 0, 0)
        out += struct.pack(">HHHH", flags, mname, mdesc, 1)
        out += struct.pack(">HI", code_name, len(attribute)) + attribute
    out += struct.pack(">H", 0)
    return bytes(out)


# ---------------------------------------------------------------------------- running


def run(cmd, cwd, timeout, stdin=None, env=None):
    start = time.perf_counter()
    try:
        done = subprocess.run(cmd, cwd=cwd, capture_output=True, timeout=timeout,
                              input=(stdin or "").encode(), env=env)
        code, out, err = done.returncode, done.stdout, done.stderr
    except subprocess.TimeoutExpired as expired:
        code, out, err = "timeout", expired.stdout or b"", expired.stderr or b""
    seconds = time.perf_counter() - start
    return {
        "exit": code,
        "stdout": out.decode("utf-8", "replace").replace("\r\n", "\n"),
        "stderr": err.decode("utf-8", "replace").replace("\r\n", "\n"),
        "seconds": round(seconds, 4),
    }


def uncaught(stderr):
    """The class and message of the exception that ended the program, from the launcher's stderr:
    `Exception in thread "main" X: m`, or the `Caused by:` under an `Error:` the launcher printed
    when the main class did not load or initialize."""
    lines = stderr.splitlines()
    for i, line in enumerate(lines):
        match = EXCEPTION_LINE.match(line)
        if match:
            return match.group(1), match.group(2)
        if line.startswith("Error:"):
            for later in lines[i + 1:]:
                cause = CAUSED_BY.match(later)
                if cause:
                    return cause.group(1), cause.group(2)
    return None, None


def summarize(result):
    exception, message = uncaught(result["stderr"])
    return {
        "exit": result["exit"],
        "stdout": result["stdout"],
        "exception": exception,
        "message": message,
        "seconds": result["seconds"],
        "stderr": result["stderr"][-600:],
    }


def verdict(oracle, got):
    """pass, or why not: the interpreter stopped, timed out, crashed, or answered differently."""
    if got["exit"] == "timeout":
        return "timeout"
    if got["exit"] == INTERPRETER_ERROR and oracle["exit"] != INTERPRETER_ERROR:
        return "unsupported"
    if isinstance(got["exit"], int) and got["exit"] != oracle["exit"] and (got["exit"] < 0 or got["exit"] >= 100):
        return "crash"  # a signal, or a Rust panic (101)
    same = (got["stdout"] == oracle["stdout"] and got["exit"] == oracle["exit"]
            and got["exception"] == oracle["exception"])
    return "pass" if same else "fail"


def commands(program, engine, paths):
    jvm = program.get("jvm", [])
    args = program.get("args", [])
    main = program["main"]
    if engine == "hotspot":
        return [str(paths["java"]), *jvm, "-cp", str(paths["classes"][program["suite"]]), main, *args], None
    if engine == "hotspot-int":
        return [str(paths["java"]), "-Xint", *jvm, "-cp", str(paths["classes"][program["suite"]]), main, *args], None
    jar = paths["jars"][program["suite"]]
    if engine == "native":
        return [str(paths["native"]), *jvm, str(jar), main, *args], None
    if engine == "wasm":
        rel = os.path.relpath(jar, paths["out"])
        return ["node", "--no-warnings", str(WASI), str(paths["wasm"]), *jvm, rel, main, *args], paths["out"]
    if engine == "rusty-jvm":
        return [str(paths["rusty"]), *jvm, "-cp", str(paths["classes"][program["suite"]]), main, *args], None
    raise ValueError(engine)


def run_program(program, engines, paths, timeout, oracle_runs):
    row = {"id": program_id(program), "category": program["category"], "needs": program.get("needs", [])}
    cmd, cwd = commands(program, "hotspot", paths)
    oracle = summarize(run(cmd, cwd, timeout, program.get("stdin"), env=java_env()))
    row["hotspot"] = oracle
    for _ in range(oracle_runs - 1):
        again = summarize(run(cmd, cwd, timeout, program.get("stdin"), env=java_env()))
        if (again["stdout"], again["exit"], again["exception"]) != (
                oracle["stdout"], oracle["exit"], oracle["exception"]):
            row["oracle_nondeterministic"] = True
    for engine in engines:
        if engine == "hotspot":
            continue
        cmd, cwd = commands(program, engine, paths)
        got = summarize(run(cmd, cwd, timeout, program.get("stdin")))
        if got["exit"] == INTERPRETER_ERROR:
            got["error"] = next((line for line in got["stderr"].splitlines() if "interpreter error" in line), "")[:400]
        got["verdict"] = verdict(oracle, got)
        row[engine] = got
    if "native" in row and "wasm" in row:
        n, w = row["native"], row["wasm"]
        row["native_wasm_agree"] = (n["stdout"], n["exit"], n["exception"]) == (w["stdout"], w["exit"], w["exception"])
    return row


def program_id(program):
    return f"{program['suite']}/{program['main']}" + (f"[{program['variant']}]" if program.get("variant") else "")


def scored(row):
    return not (set(row["needs"]) & EXCLUDED_NEEDS) and not row.get("oracle_nondeterministic")


def report(rows, engines):
    engines = [e for e in engines if e != "hotspot"]
    categories = sorted({row["category"] for row in rows})
    header = f"{'category':<22} {'programs':>8} {'scored':>6}" + "".join(f" {e:>14}" for e in engines)
    if "native" in engines and "wasm" in engines:
        header += f" {'native=wasm':>12}"
    print(header)
    totals = {"programs": 0, "scored": 0, **{e: 0 for e in engines}, "agree": 0}
    for category in categories + ["TOTAL"]:
        picked = rows if category == "TOTAL" else [r for r in rows if r["category"] == category]
        counted = [r for r in picked if scored(r)]
        line = f"{category:<22} {len(picked):>8} {len(counted):>6}"
        for engine in engines:
            passed = sum(1 for r in counted if r.get(engine, {}).get("verdict") == "pass")
            share = f"{passed}/{len(counted)}" + (f" {100 * passed / len(counted):3.0f}%" if counted else "")
            line += f" {share:>14}"
        if "native" in engines and "wasm" in engines:
            agree = sum(1 for r in picked if r.get("native_wasm_agree"))
            line += f" {agree:>5}/{len(picked):<6}"
        print(line)
    print()
    for engine in engines:
        verdicts = {}
        for row in rows:
            if scored(row) and engine in row:
                verdicts[row[engine]["verdict"]] = verdicts.get(row[engine]["verdict"], 0) + 1
        print(f"{engine}: " + ", ".join(f"{k} {v}" for k, v in sorted(verdicts.items())))
    flaky = [r["id"] for r in rows if r.get("oracle_nondeterministic")]
    if flaky:
        print(f"left out, HotSpot's own output varies between runs: {len(flaky)}")
    excluded = [r for r in rows if set(r["needs"]) & EXCLUDED_NEEDS]
    if excluded:
        print(f"left out, need {', '.join(sorted(EXCLUDED_NEEDS))}: {len(excluded)}")


# ---------------------------------------------------------------------------- mod-shaped jars


def run_mods(home, manifest, paths, engines, timeout, out):
    """Each mod-shaped jar: compiled against the stubs, jarred without them, run for its role."""
    spec = manifest["mods"]
    src = CORPUS / spec["dir"]
    stubs = src / spec["stubs"]
    rows = []
    for mod in spec["jars"]:
        work = out / "mods" / mod["name"]
        classes = work / "classes"
        if work.exists():
            shutil.rmtree(work)
        classes.mkdir(parents=True)
        mod_dir = src / mod["dir"]
        sources = sorted(mod_dir.rglob("*.java"))
        javac = [str(home / "bin" / "javac"), "-nowarn", "-encoding", "UTF-8", "-d", str(classes),
                 "-sourcepath", str(stubs)]
        done = subprocess.run([*javac, *map(str, sources)], capture_output=True, text=True, env=java_env())
        if done.returncode != 0:
            sys.exit(f"java_conformance: javac failed for mod {mod['name']}:\n{done.stderr[-3000:]}")
        packages = tuple(p.replace(".", "/") + "/" for p in mod["packages"])
        resources = {p.relative_to(mod_dir).as_posix(): p.read_bytes()
                     for p in sorted(mod_dir.rglob("*")) if p.is_file() and p.suffix != ".java"}
        jar = make_jar(classes, work / f"{mod['name']}.jar", include=lambda n: n.startswith(packages),
                       resources=resources)
        row = {"name": mod["name"], "role": mod["role"]}
        for engine in [e for e in engines if e in ("native", "wasm")]:
            if engine == "native":
                cmd, cwd = [str(paths["native"]), "--mod", mod["role"], str(jar)], None
            else:
                cmd, cwd = ["node", "--no-warnings", str(WASI), str(paths["wasm"]), "--mod", mod["role"],
                            os.path.relpath(jar, out)], out
            result = run(cmd, cwd, timeout)
            try:
                got = json.loads(result["stdout"].strip().splitlines()[-1])
            except (ValueError, IndexError):
                got = {"status": "crash", "error": result["stderr"][-400:]}
            row[engine] = {"report": got, "problems": mod_problems(mod["expect"], got)}
        if "native" in row and "wasm" in row:
            row["native_wasm_agree"] = row["native"]["report"] == row["wasm"]["report"]
        rows.append(row)
    for row in rows:
        for engine in ("native", "wasm"):
            if engine in row:
                problems = row[engine]["problems"]
                status = "pass" if not problems else "FAIL: " + "; ".join(problems)
                print(f"mod {row['name']:<20} {row['role']:<7} {engine:<7} {status}")
        if "native_wasm_agree" in row and not row["native_wasm_agree"]:
            print(f"mod {row['name']:<20} native and wasm reports differ")
    return rows


def mod_problems(expect, got):
    """What the run's report gets wrong against the jar's expected Host-side outcome."""
    problems = []
    for key in ("status", "entrypoints", "completed"):
        if key in expect and got.get(key) != expect[key]:
            problems.append(f"{key} {got.get(key)!r}, expected {expect[key]!r}"
                            + (f" ({got.get('error')})" if key in ("status", "completed") and got.get("error") else ""))
    if "registrations" in expect:
        have = {(r["registry"], r["id"]): r.get("declaration") for r in got.get("registrations", [])}
        for want in expect["registrations"]:
            key = (want["registry"], want["id"])
            if key not in have:
                problems.append(f"no registration {key[0]} {key[1]}")
                continue
            declaration = have[key]
            wanted = want.get("declaration")
            if wanted is None:
                continue
            if declaration is None:
                problems.append(f"{key[1]} is opaque, expected a typed {wanted.get('kind')}")
                continue
            for field, value in wanted.items():
                if field == "properties":
                    for prop, expected in value.items():
                        actual = declaration.get("properties", {}).get(prop)
                        if actual != expected:
                            problems.append(f"{key[1]} {prop}={actual!r}, expected {expected!r}")
                elif declaration.get(field) != value:
                    problems.append(f"{key[1]} {field}={declaration.get(field)!r}, expected {value!r}")
        extra = len(got.get("registrations", [])) - len(expect["registrations"])
        if expect.get("exact") and extra > 0:
            problems.append(f"{extra} registration(s) more than expected")
        if expect.get("ordered"):
            want = [(r["registry"], r["id"]) for r in expect["registrations"]]
            seen = [(r["registry"], r["id"]) for r in got.get("registrations", []) if (r["registry"], r["id"]) in set(want)]
            if seen != [key for key in want if key in seen]:
                problems.append("registration order " + ", ".join(i for _, i in seen))
    return problems


# ---------------------------------------------------------------------------- real mods


def run_real_mods(directory, paths, engines, roles, timeout):
    """Entrypoints that complete and registrations that become typed declarations, per local jar."""
    jars = sorted(pathlib.Path(directory).expanduser().glob("*.jar"))
    if not jars:
        sys.exit(f"java_conformance: no jars in {directory}")
    rows = []
    for jar in jars:
        for role in roles:
            row = {"jar": jar.name, "role": role}
            for engine in [e for e in engines if e in ("native", "wasm")]:
                if engine == "native":
                    cmd, cwd = [str(paths["native"]), "--mod", role, str(jar)], None
                else:
                    cmd, cwd = ["node", "--no-warnings", str(WASI), str(paths["wasm"]), "--mod", role,
                                os.path.relpath(jar, jar.parent)], jar.parent
                result = run(cmd, cwd, timeout)
                try:
                    got = json.loads(result["stdout"].strip().splitlines()[-1])
                except (ValueError, IndexError):
                    got = {"status": "crash", "error": (result["stderr"] or str(result["exit"]))[-300:]}
                got["seconds"] = result["seconds"]
                row[engine] = got
            rows.append(row)
    for engine in [e for e in engines if e in ("native", "wasm")]:
        print(f"{engine}:")
        print(f"  {'jar':<44} {'role':<7} {'status':<8} {'entry':>5} {'done':>5} {'regs':>5} {'typed':>5} {'ms':>7}  first error")
        totals = {"jars": 0, "entry": 0, "done": 0, "regs": 0, "typed": 0, "ok": 0}
        for row in rows:
            got = row.get(engine)
            if got is None:
                continue
            regs = got.get("registrations", [])
            typed = sum(1 for r in regs if r.get("declaration"))
            if got.get("status") in ("ok", "error"):
                totals["jars"] += 1
                totals["entry"] += got.get("entrypoints", 0)
                totals["done"] += got.get("completed", 0)
                totals["regs"] += len(regs)
                totals["typed"] += typed
                totals["ok"] += got.get("status") == "ok"
            print(f"  {row['jar'][:44]:<44} {row['role']:<7} {got.get('status', '?'):<8} "
                  f"{got.get('entrypoints', 0):>5} {got.get('completed', 0):>5} {len(regs):>5} {typed:>5} "
                  f"{1000 * got['seconds']:>7.0f}  {(got.get('error') or '')[:90]}")
        print(f"  jars with an entrypoint for the role: {totals['jars']}, all entrypoints completed: {totals['ok']}; "
              f"entrypoints {totals['done']}/{totals['entry']} completed; registrations {totals['regs']}, typed {totals['typed']}")
    return rows


# ---------------------------------------------------------------------------- benchmarks


def run_bench(manifest, paths, engines, repeat, timeout):
    """Wall time per engine, median of `repeat` runs, and the same less the engine's empty-program
    startup (the `startup` program)."""
    programs = [p for p in manifest["programs"] if p["suite"] == "bench"]
    startup = next(p for p in programs if p.get("startup"))
    engines = [e for e in ["hotspot", "hotspot-int", "native", "wasm", "rusty-jvm"]
               if e in engines or (e == "hotspot-int" and "hotspot" in engines)]
    if paths.get("rusty") and "rusty-jvm" not in engines:
        engines.append("rusty-jvm")
    rows = []
    for program in programs:
        row = {"id": program_id(program)}
        reference = None
        for engine in engines:
            cmd, cwd = commands(program, engine, paths)
            times, result = [], None
            for _ in range(repeat):
                result = run(cmd, cwd, timeout, env=java_env() if engine.startswith("hotspot") else None)
                times.append(result["seconds"])
            summary = summarize(result)
            if engine == "hotspot":
                reference = summary
            ok = reference is not None and (summary["stdout"], summary["exit"]) == (reference["stdout"], reference["exit"])
            row[engine] = {"seconds": statistics.median(times), "ok": ok,
                           "exit": summary["exit"]}
        rows.append(row)
    base = {e: next(r for r in rows if r["id"] == program_id(startup))[e]["seconds"] for e in engines}
    print(f"{'benchmark':<34}" + "".join(f" {e:>16}" for e in engines))
    for row in rows:
        line = f"{row['id'].rsplit('.', 1)[-1][:34]:<34}"
        for engine in engines:
            cell = row[engine]
            mark = "" if cell["ok"] else " x"
            net = cell["seconds"] - (0 if row["id"] == program_id(startup) else base[engine])
            line += f" {1000 * cell['seconds']:>7.0f}/{1000 * max(net, 0):>6.0f}{mark:<2}"
        print(line)
    print("milliseconds: wall time / less the engine's startup (the empty program); x = wrong output")
    return rows


# ---------------------------------------------------------------------------- scanning


NEEDS = {
    "files": r"java\.io\.File\b|FileInputStream|FileOutputStream|RandomAccessFile|FileReader|FileWriter|"
             r"java\.nio\.file|java\.nio\.channels|\bFiles\.|\bPaths\.|deleteOnExit|new File\(",
    "network": r"java\.net\.(?!URI\b)",
    "jni": r"System\.loadLibrary|System\.load\(|\bnative\s+\w+[\w<>\[\]]*\s+\w+\s*\(",
    "process": r"ProcessBuilder|Runtime\.getRuntime\(\)\.exec|ProcessHandle",
    "env": r"System\.getenv",
    "jdk-internal": r"jdk\.internal\.|sun\.misc\.|^package java\.",
    "stdin": r"System\.in\b",
}


def scan(manifest):
    """Mains in the corpus that the manifest does not list, with the needs their source suggests."""
    listed = {(p["suite"], p["main"]) for p in manifest["programs"]}
    found = []
    for suite, spec in manifest["suites"].items():
        if spec.get("generated") or "dir" not in spec:
            continue
        for path in sorted((CORPUS / spec["dir"]).rglob("*.java")):
            text = path.read_text(errors="replace")
            if not re.search(r"\bvoid\s+main\s*\(", text):
                continue
            package = re.search(r"^package\s+([\w.]+);", text, re.M)
            main = (package.group(1) + "." if package else "") + path.stem
            if (suite, main) in listed:
                continue
            needs = sorted(k for k, pattern in NEEDS.items() if re.search(pattern, text, re.M))
            category = package.group(1).split(".")[1] if package and package.group(1).startswith("samples.") else suite
            found.append({"suite": suite, "main": main, "category": category, "needs": needs})
    return found


# ---------------------------------------------------------------------------- main


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--java-home")
    parser.add_argument("--engines", default="hotspot,native,wasm")
    parser.add_argument("--filter", help="regex over program ids")
    parser.add_argument("--jobs", type=int, default=os.cpu_count() or 2)
    parser.add_argument("--timeout", type=float, default=30)
    parser.add_argument("--oracle-runs", type=int, default=2, help="HotSpot runs per program (2 finds unstable output)")
    parser.add_argument("--out", default=str(ROOT / ".local/java-conformance"))
    parser.add_argument("--json", help="write every run's result here")
    parser.add_argument("--no-build", action="store_true", help="use the runners already built")
    parser.add_argument("--mods", action="store_true", help="the mod-shaped jars instead of the programs")
    parser.add_argument("--real-mods", metavar="DIR", help="run the jars in DIR for each role instead")
    parser.add_argument("--roles", default="server,client")
    parser.add_argument("--bench", action="store_true", help="time the benchmark programs instead")
    parser.add_argument("--repeat", type=int, default=3)
    parser.add_argument("--rusty-jvm", metavar="BIN", help="rusty-jvm's binary, as one more benchmark engine")
    parser.add_argument("--scan", action="store_true", help="print corpus mains missing from the manifest")
    args = parser.parse_args()

    manifest = load_manifest()
    if args.scan:
        print(json.dumps(scan(manifest), indent=1))
        return 0
    engines = [e.strip() for e in args.engines.split(",") if e.strip()]
    if "wasm" in engines and not shutil.which("node"):
        print("java_conformance: node not found, skipping the wasm runs", file=sys.stderr)
        engines.remove("wasm")
    out = pathlib.Path(args.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    native, wasm = build_runners(engines, args.no_build)
    paths = {"native": native, "wasm": wasm, "out": out, "rusty": args.rusty_jvm}
    if args.real_mods:
        rows = run_real_mods(args.real_mods, paths, engines, args.roles.split(","), args.timeout)
        if args.json:
            pathlib.Path(args.json).write_text(json.dumps(rows, indent=1))
        return 0
    home = java_home(args.java_home)
    paths["java"] = home / "bin" / "java"
    if args.mods:
        rows = run_mods(home, manifest, paths, engines, args.timeout, out)
        if args.json:
            pathlib.Path(args.json).write_text(json.dumps(rows, indent=1))
        return 0 if all(not row[e]["problems"] for row in rows for e in ("native", "wasm") if e in row) else 1

    programs = manifest["programs"]
    if args.bench:
        programs = [p for p in programs if p["suite"] == "bench"]
    else:
        programs = [p for p in programs if p["suite"] != "bench"]
    if args.filter:
        programs = [p for p in programs if re.search(args.filter, program_id(p))]
    suites = sorted({p["suite"] for p in programs})
    paths["classes"] = {s: compile_suite(home, s, manifest["suites"][s], out) for s in suites}
    paths["jars"] = {s: make_jar(paths["classes"][s], out / s / f"{s}.jar") for s in suites}
    if args.bench:
        rows = run_bench(manifest, paths, engines, args.repeat, args.timeout * 4)
        if args.json:
            pathlib.Path(args.json).write_text(json.dumps(rows, indent=1))
        return 0
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        rows = list(pool.map(lambda p: run_program(p, engines, paths, args.timeout, args.oracle_runs), programs))
    report(rows, engines)
    if args.json:
        pathlib.Path(args.json).write_text(json.dumps(rows, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Differential fuzz between the native Java interpreter and a real JVM.

    java_differential_fuzz.py [--fixtures N] [--seed N] [--timeout S]
                              [--mutations N] [--bin-dir DIR] [--json]
                              [--output PATH] [--keep]
    java_differential_fuzz.py --score-only [--probe P] [--ledger L] [--fuzz F]
                              [--previous S] [--label DATE] [--output PATH]
    java_differential_fuzz.py --self-test

One fixture is a small Fabric jar whose entrypoint computes strings through a
randomly composed set of Java surfaces (arithmetic, strings, collections,
records, enums, exceptions, math, ...) and registers each result through
``Registry.register(Registries.ITEM, Identifier..., ...)``.  The same compiled
classfiles then run twice:

* ``horizon-native-java --json`` answers with the registration rows the host
  recorded;
* the JVM runs ``demo.Entry`` against a small ``net.minecraft`` stub that
  prints the same rows.

The two row lists are the observable output.  They must agree; a difference is
a host gap in the native path, which is exactly what the nightly job wants to
see instead of guessing.  Fixtures are generated from a deterministic seed and
then structurally mutated (insert/replace/drop probes) so a run is reproducible
byte for byte on the same toolchain.

Exit 0 even when native and JVM diverge: a divergence is the report, not a
harness failure.  Exit 1 for a harness error, 2 when the toolchain is missing.

``--score-only`` folds the nightly probe, ledger and fuzz reports into
``compatibility-score.json`` and prints the Markdown summary the workflow
appends to its step summary.  ``--previous`` turns the previous run's score
into a trend.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import random
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import zipfile
from collections import Counter
from dataclasses import dataclass, field
from datetime import date
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))
import lab  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]

DEFAULT_FIXTURES = 8
DEFAULT_SEED = 20261106
DEFAULT_TIMEOUT = 20.0
DEFAULT_BUILD_TIMEOUT = 900.0
DEFAULT_MUTATIONS = 3
MAX_FIXTURES = 64
MAX_MUTATIONS = 8
MAX_SOURCE_BYTES = 256 << 10
OUTPUT_CAP = 256 << 10
TAIL = 240

STATUSES = ("ran", "refused", "skipped", "needs-loader")

# The native probe's process exit code is meaningful, but its JSON is the
# report.  A refused discovery and a failed execution are different causes.
DISCOVERY_STAGES = {"discover", "load"}

COMMON_IMPORTS = """import java.util.*;
import java.util.function.*;
import java.util.stream.*;
import net.minecraft.core.Registry;
import net.minecraft.core.registries.Registries;
import net.minecraft.resources.Identifier;
"""

# Compile-only stubs.  The jar never contains these classes: javac needs the
# signatures, and leaving them out is what makes each call cross the host.
COMPILE_STUBS = {
    "net/minecraft/resources/Identifier.java": """package net.minecraft.resources;
public final class Identifier {
    public static Identifier fromNamespaceAndPath(String namespace, String path) { throw new UnsupportedOperationException(); }
    public static Identifier tryParse(String value) { throw new UnsupportedOperationException(); }
    @Override public String toString() { throw new UnsupportedOperationException(); }
    @Override public boolean equals(Object other) { throw new UnsupportedOperationException(); }
    @Override public int hashCode() { throw new UnsupportedOperationException(); }
}
""",
    "net/minecraft/core/Registry.java": """package net.minecraft.core;
import net.minecraft.resources.Identifier;
public final class Registry<T> {
    public static <T> T register(Registry<T> registry, Identifier id, T value) { throw new UnsupportedOperationException(); }
}
""",
    "net/minecraft/core/registries/Registries.java": """package net.minecraft.core.registries;
public final class Registries {
    public static final net.minecraft.core.Registry<Object> ITEM = null;
}
""",
}

# The JVM reference host.  It answers the same calls the native StdHost models
# and prints one stable row per registration; that print is the JVM side of the
# differential diff.
RUNTIME_STUBS = {
    "net/minecraft/resources/Identifier.java": """package net.minecraft.resources;
public final class Identifier {
    private final String value;
    private Identifier(String value) { this.value = value; }
    public static Identifier fromNamespaceAndPath(String namespace, String path) { return new Identifier(namespace + ":" + path); }
    public static Identifier tryParse(String value) { return new Identifier(value); }
    @Override public String toString() { return value; }
    @Override public boolean equals(Object other) { return other instanceof Identifier && ((Identifier) other).value.equals(value); }
    @Override public int hashCode() { return value.hashCode(); }
}
""",
    "net/minecraft/core/Registry.java": """package net.minecraft.core;
import net.minecraft.resources.Identifier;
public final class Registry<T> {
    private final String name;
    public Registry(String name) { this.name = name; }
    public static <T> T register(Registry<T> registry, Identifier id, T value) {
        System.out.println("@register\\t" + registry.name + "\\t" + id);
        return value;
    }
}
""",
    "net/minecraft/core/registries/Registries.java": """package net.minecraft.core.registries;
public final class Registries {
    public static final net.minecraft.core.Registry<Object> ITEM =
            new net.minecraft.core.Registry<Object>("minecraft:item");
}
""",
}


def _literal(rng: random.Random, low: int = -50, high: int = 50) -> int:
    return rng.randint(low, high)


def _positive(rng: random.Random, low: int = 1, high: int = 30) -> int:
    return rng.randint(low, high)


# ---------------------------------------------------------------------------
# Probe templates.  Each returns (label, body lines, result expression, extra
# type/method blocks).  "core" probes stick to the surface the runtime already
# claims; "candidate" probes reach the next layer, where a neutral answer is a
# real host gap rather than a generator bug.
# ---------------------------------------------------------------------------


def probe_int_arith(rng: random.Random):
    a, b, c = _literal(rng), _literal(rng), _literal(rng)
    shift = rng.randint(0, 5)
    body = [
        f"int a = {a}, b = {b}, c = {c};",
        f"int v = (a * b + c * 7) ^ (a >>> {shift});",
        f"v += (b << {rng.randint(0, 4)});",
        f"v ^= (v >>> {rng.randint(1, 7)});",
    ]
    return "int-arith", body, "Integer.toString(v)", []


def probe_long_arith(rng: random.Random):
    a, b = _literal(rng, -10**6, 10**6), _positive(rng, 1, 10**6)
    body = [
        f"long a = {a}L, b = {b}L;",
        f"long v = a * b + (a << {rng.randint(0, 6)}) - b;",
        f"v ^= v >>> {rng.randint(1, 13)};",
        f"v += (v & 0xFFFFL);",
    ]
    return "long-arith", body, "Long.toString(v)", []


def probe_double_math(rng: random.Random):
    a = round(rng.uniform(-20, 20), 3)
    b = round(rng.uniform(0.5, 9.0), 3)
    body = [
        f"double a = {a}D, b = {b}D;",
        "double v = (a * b) / (b + 1.0D);",
        "v += Math.abs(a - b);",
        "v = Math.floor(v * 100.0D) / 100.0D;",
    ]
    return "double-math", body, "Double.toString(v)", []


def probe_math_ints(rng: random.Random):
    a, b = _literal(rng), _positive(rng, 2, 19)
    body = [
        f"int a = {a}, b = {b};",
        "int v = Math.max(a, b) + Math.abs(a - b) + Math.floorMod(a, b) + Math.floorDiv(b, 3);",
        "long w = Math.round(Math.sqrt(Math.abs(a)) * 1000.0D);",
    ]
    return "math-ints", body, 'v + ":" + Long.toString(w)', []


def probe_strings(rng: random.Random):
    a, b = _positive(rng), _positive(rng)
    body = [
        f'String s = "Alpha-Beta_" + ({a} * {b});',
        'String t = s.toLowerCase().replace(\'a\', \'x\').trim();',
        'int i = t.indexOf("betx");',
        'String u = t.substring(0, Math.min(4, t.length())).toUpperCase();',
    ]
    return "strings", body, 'u + "/" + i + "/" + t.length() + "/" + t', []


def probe_string_split(rng: random.Random):
    a = _positive(rng)
    body = [
        f'String[] parts = ("z|a|m|n{a}").split("\\\\|");',
        "Arrays.sort(parts);",
        "String joined = String.join(\",\", parts);",
        f"String[] limited = \"a,b,c,{a}\".split(\",\", 2);",
    ]
    return "string-split", body, 'joined + "/" + limited.length + "/" + limited[1]', []


def probe_arrays(rng: random.Random):
    a, b, c = _literal(rng), _literal(rng), _literal(rng)
    body = [
        f"int[] values = {{{c}, {a}, {b}, {a + c}}};",
        "Arrays.sort(values);",
        f"int index = Arrays.binarySearch(values, {b});",
        f"int[] copy = Arrays.copyOf(values, {2 + (a & 3)});",
    ]
    return "arrays", body, 'Arrays.toString(values) + ":" + index + ":" + Arrays.toString(copy)', []


def probe_collections(rng: random.Random):
    n = _positive(rng, 2, 6)
    lines = ["Map<String, Integer> map = new TreeMap<>();"]
    for index in range(n):
        lines.append(f'map.put("k{index}_{_positive(rng)}", {_literal(rng)});')
    lines += [
        "StringBuilder builder = new StringBuilder();",
        "for (Map.Entry<String, Integer> entry : map.entrySet()) {",
        '    builder.append(entry.getKey()).append("=").append(entry.getValue()).append(";");',
        "}",
    ]
    return "collections", lines, "builder.toString()", []


def probe_list(rng: random.Random):
    a = _positive(rng)
    body = [
        'List<String> list = new ArrayList<>();',
        'list.add("delta");',
        'list.add("alpha");',
        f'list.add("k{a}");',
        "Collections.sort(list);",
        'list.add("omega");',
    ]
    return "list", body, 'String.join("", list) + ":" + list.size()', []


def probe_parse(rng: random.Random):
    a = _positive(rng, 1, 999)
    body = [
        f'int a = Integer.parseInt("" + {a});',
        f'long b = Long.parseLong("-{a}");',
        f'double d = Double.parseDouble("{a}.5");',
        'boolean z = Boolean.parseBoolean("TRUE");',
    ]
    return "parse", body, 'a + "/" + b + "/" + Double.toString(d) + "/" + z', []


def probe_switch(rng: random.Random):
    a, b = _literal(rng), _positive(rng)
    body = [
        f"int n = Math.abs({a}) % 5;",
        "String s;",
        "switch (n) {",
        '    case 0: s = "zero"; break;',
        '    case 1: case 2: s = "low"; break;',
        '    default: s = "high";',
        "}",
        f'switch ("k" + ({b} % 3)) {{',
        '    case "k0": s += "-k0"; break;',
        '    case "k1": s += "-k1"; break;',
        '    default: s += "-other";',
        "}",
    ]
    return "switch", body, 's + n', []


def probe_optional(rng: random.Random):
    a = _positive(rng)
    body = [
        f'Optional<String> value = Optional.of("v{a}").map(x -> x + "!").filter(x -> x.length() > 2);',
        'String none = Optional.<String>empty().orElse("none");',
    ]
    return "optional", body, 'value.orElse("missing") + ":" + value.isPresent() + ":" + none', []


def probe_record(rng: random.Random):
    a, b = _literal(rng), _literal(rng)
    body = [
        f"Pair pair = new Pair({a}, {b});",
        "Pair other = new Pair(pair.left() + 1, pair.right() - 1);",
    ]
    return "record", body, 'pair.left() + "-" + pair.right() + "-" + pair.toString() + "-" + other', [
        "    record Pair(int left, int right) {}"
    ]


def probe_enum(rng: random.Random):
    a = _positive(rng)
    body = [
        f"Direction d = Direction.values()[{a} & 3];",
        "Direction next = d.next();",
    ]
    return "enum", body, 'd.name() + ":" + d.ordinal() + ":" + d.axis() + ":" + next.name()', [
        "    enum Direction {",
        "        NORTH(0), EAST(1), SOUTH(2), WEST(3);",
        "        private final int axis;",
        "        Direction(int axis) { this.axis = axis; }",
        "        int axis() { return axis; }",
        "        Direction next() { return values()[(ordinal() + 1) & 3]; }",
        "    }"
    ]


def probe_inheritance(rng: random.Random):
    a = _literal(rng)
    body = [
        f"Base base = ({a} & 1) == 0 ? new Left() : new Right();",
    ]
    return "inheritance", body, 'base.label() + ":" + base.value() + ":" + base.describe()', [
        "    interface Base {",
        "        String label();",
        "        int value();",
        '        default String describe() { return label() + "/" + value(); }',
        "    }",
        "    static final class Left implements Base {",
        '        public String label() { return "left"; }',
        "        public int value() { return 19; }",
        "    }",
        "    static final class Right implements Base {",
        '        public String label() { return "right"; }',
        "        public int value() { return 23; }",
        "    }"
    ]


def probe_exceptions(rng: random.Random):
    a, b = _literal(rng), _positive(rng)
    body = [
        f"int v = {a};",
        f"try {{ v = v / ({b} % 3); }} catch (ArithmeticException e) {{ v = -1; }}",
        'try { throw new IllegalStateException("x"); } catch (IllegalStateException e) { v += e.getMessage().length(); }',
        "finally { v += 1; }",
    ]
    return "exceptions", body, "Integer.toString(v)", []


def probe_boxing(rng: random.Random):
    a, b = _literal(rng, -200, 200), _literal(rng, -200, 200)
    body = [
        f"Integer a = {a}, b = {b};",
        "int sum = a + b;",
        f"boolean equal = a.equals(Integer.valueOf({a}));",
        "Integer cached = 127;",
        "boolean same = cached == Integer.valueOf(127);",
    ]
    return "boxing", body, 'sum + ":" + equal + ":" + same + ":" + a.compareTo(b)', []


def probe_chars(rng: random.Random):
    a = _positive(rng)
    body = [
        f"char ch = (char) ('a' + ({a} & 7));",
        "boolean digit = Character.isDigit(ch);",
        "int numeric = Character.getNumericValue('7');",
        "String text = new String(new char[] {ch, '!', '?'});",
    ]
    return "chars", body, 'Character.toUpperCase(ch) + ":" + digit + ":" + numeric + ":" + text', []


def probe_static_state(rng: random.Random):
    a = _positive(rng)
    body = [
        f"Counter.reset({a});",
        "Counter.bump();",
        "Counter.bump();",
    ]
    return "static-state", body, "Integer.toString(Counter.value())", [
        "    static final class Counter {",
        "        private static int value;",
        "        static void reset(int seed) { value = seed; }",
        "        static void bump() { value += 7; }",
        "        static int value() { return value; }",
        "    }"
    ]


def probe_varargs(rng: random.Random):
    a, b, c = _literal(rng), _literal(rng), _literal(rng)
    return "varargs", [], f"Integer.toString(total({a}, {b}, {c}))", [
        "    private static int total(int... values) {",
        "        int sum = 0;",
        "        for (int value : values) { sum += value; }",
        "        return sum;",
        "    }"
    ]


def probe_recursion(rng: random.Random):
    n = rng.randint(3, 10)
    return "recursion", [], f"Integer.toString(fib({n}))", [
        "    private static int fib(int n) {",
        "        return n < 2 ? n : fib(n - 1) + fib(n - 2);",
        "    }"
    ]


def probe_string_format(rng: random.Random):
    a = _positive(rng)
    return "string-format", [], f'String.format("%s-%d-%.2f", "x", {a}, 1.5D)', []


def probe_stream(rng: random.Random):
    a = _positive(rng)
    return "stream", [], (
        f'Integer.toString(IntStream.range(0, 5).map(i -> i * {a}).sum())'
    ), []


def probe_uuid(rng: random.Random):
    a = _positive(rng)
    return "uuid", [], (
        f'UUID.nameUUIDFromBytes(("fuzz-" + {a}).getBytes()).toString()'
    ), []


CORE_PROBES = (
    probe_int_arith, probe_long_arith, probe_double_math, probe_math_ints,
    probe_strings, probe_string_split, probe_arrays, probe_collections,
    probe_list, probe_parse, probe_switch, probe_optional, probe_record,
    probe_enum, probe_inheritance, probe_exceptions, probe_boxing, probe_chars,
    probe_static_state, probe_varargs, probe_recursion,
)
CANDIDATE_PROBES = (probe_string_format, probe_stream, probe_uuid)
PROBE_POOL = {function.__name__: function for function in CORE_PROBES + CANDIDATE_PROBES}


# ---------------------------------------------------------------------------
# Fixture generation
# ---------------------------------------------------------------------------


@dataclass
class Fixture:
    name: str
    seed: int
    probes: list[str]
    mutations: list[str] = field(default_factory=list)
    parameter_seed: int = 0


def render_fixture(probes: list[tuple[str, list[str], str, list[str]]]) -> str:
    types: list[str] = []
    seen_types: set[str] = set()
    methods: list[str] = []
    calls: list[str] = []
    for index, (_, body, result, extras) in enumerate(probes):
        # One template contributes at most one nested type or helper block;
        # the list is its lines, never a list of blocks.
        if extras:
            key = "\n".join(extras)
            if key not in seen_types:
                seen_types.add(key)
                types.extend(["", *extras])
        lines = [f"    private static String p{index}() {{"]
        lines.extend(f"        {line}" if line else "" for line in body)
        lines.append(f"        return {result};")
        lines.append("    }")
        methods.append("\n".join(lines))
        calls.extend([
            "        String r%d;" % index,
            "        try { r%d = p%d(); } catch (RuntimeException e) { r%d = \"rt\"; } "
            "catch (Throwable t) { r%d = \"err\"; }" % (index, index, index, index),
            "        emit(%d, r%d);" % (index, index),
        ])

    parts = [
        "package demo;",
        "",
        COMMON_IMPORTS,
        "public final class Entry {",
        "    public static void main(String[] args) { new Entry().onInitialize(); }",
        "",
        "    public void onInitialize() {",
        *calls,
        "    }",
        "",
        "    private static void emit(int index, String value) {",
        '        Registry.register(Registries.ITEM,',
        '                Identifier.fromNamespaceAndPath("fuzz", "p" + index + "=" + value),',
        "                new Object());",
        "    }",
        "",
        *methods,
        "",
        *types,
        "}",
        "",
    ]
    return "\n".join(parts)


def apply_mutations(
    rng: random.Random,
    probe: str,
    parameter_seed: int,
    budget: int,
) -> tuple[str, list[str], int]:
    """Mutate one fixture's recipe, deterministically and within bounds.

    A swap moves the fixture to another language surface; a reseed keeps the
    surface but changes every literal the template draws.  Candidates are only
    reached through a mutation, so the baseline fixtures stay a regression
    signal instead of all failing at verification at once.
    """
    current = probe
    applied: list[str] = []
    for _ in range(max(0, min(budget, MAX_MUTATIONS))):
        roll = rng.random()
        if roll < 0.3:
            candidate = rng.choice(CANDIDATE_PROBES).__name__
        elif roll < 0.55:
            candidate = rng.choice(CORE_PROBES).__name__
        else:
            parameter_seed = rng.randrange(1, 1 << 31)
            applied.append(f"reseed:{parameter_seed}")
            continue
        if candidate != current:
            applied.append(f"replace:{current}->{candidate}")
            current = candidate
    return current, applied, parameter_seed


def generate_fixture(seed: int, *, mutations: int = DEFAULT_MUTATIONS) -> Fixture:
    rng = random.Random(seed)
    # One probe per fixture: a probe the native path refuses at verification
    # must not hide every other result in the same jar.
    probe = rng.choice(CORE_PROBES).__name__
    parameter_seed = rng.randrange(1, 1 << 31)
    probe, applied, parameter_seed = apply_mutations(rng, probe, parameter_seed, mutations)
    return Fixture(
        name=f"fuzz-{seed}",
        seed=seed,
        probes=[probe],
        mutations=applied,
        parameter_seed=parameter_seed,
    )


def fixture_for(seed: int, index: int, *, mutations: int = DEFAULT_MUTATIONS) -> tuple[Fixture, list[tuple[str, list[str], str, list[str]]]]:
    fixture = generate_fixture(seed + index * 7919, mutations=mutations)
    rendered = []
    for position, name in enumerate(fixture.probes):
        # `hash()` is randomized per process, so seed the probe parameters from
        # a stable sum instead; the same seed must produce the same source.
        stable = sum((offset + 1) * ord(char) for offset, char in enumerate(name))
        rng = random.Random(fixture.parameter_seed + stable * 31 + position)
        rendered.append(PROBE_POOL[name](rng))
    return fixture, rendered


# ---------------------------------------------------------------------------
# Running one fixture on both sides
# ---------------------------------------------------------------------------


@dataclass
class Side:
    ok: bool
    registrations: list[tuple[str, str]] = field(default_factory=list)
    stage: str | None = None
    error: str | None = None
    exit: int | None = None
    timed_out: bool = False
    detail: str = ""


@dataclass
class Observation:
    fixture: str
    seed: int
    mutations: list[str]
    status: str
    reason: str
    native: Side
    jvm: Side
    diff: dict[str, Any] = field(default_factory=dict)

    def json(self) -> dict[str, Any]:
        return {
            "fixture": self.fixture,
            "seed": self.seed,
            "mutations": self.mutations,
            "status": self.status,
            "reason": self.reason,
            "native": {
                "status": "ok" if self.native.ok else (self.native.stage or "error"),
                "registrations": [list(row) for row in self.native.registrations],
                "error": self.native.error,
                "timed_out": self.native.timed_out,
            },
            "jvm": {
                "status": "ok" if self.jvm.ok else "error",
                "registrations": [list(row) for row in self.jvm.registrations],
                "timed_out": self.jvm.timed_out,
            },
            "diff": self.diff,
        }


def tail(text: str, limit: int = TAIL) -> str:
    value = " ".join(text.replace("\x00", "").split())
    return value[-limit:] if value else ""


def run_bounded(command: list[str], timeout: float, cwd: Path) -> tuple[int, bool, str, str]:
    """Run one child in its own process group, cap both streams, kill on timeout."""
    proc = subprocess.Popen(
        command,
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    stdout: list[bytes] = []
    stderr: list[bytes] = []

    def drain(stream, sink: list[bytes]) -> None:
        while True:
            chunk = stream.read(8192)
            if not chunk:
                return
            if sum(len(item) for item in sink) < OUTPUT_CAP:
                sink.append(chunk)

    import threading

    readers = [
        threading.Thread(target=drain, args=(proc.stdout, stdout), daemon=True),
        threading.Thread(target=drain, args=(proc.stderr, stderr), daemon=True),
    ]
    for reader in readers:
        reader.start()
    timed_out = False
    try:
        code = proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        with contextlib.suppress(ProcessLookupError):
            os.killpg(proc.pid, signal.SIGKILL)
        code = proc.wait()
    for reader in readers:
        reader.join(timeout=5)
    for stream in (proc.stdout, proc.stderr):
        if stream is not None:
            stream.close()
    return (
        code,
        timed_out,
        b"".join(stdout).decode("utf-8", "replace"),
        b"".join(stderr).decode("utf-8", "replace"),
    )


REGISTER_LINE = re.compile(r"^@register\t([^\t]+)\t(.+)$")


def jvm_registrations(stdout: str) -> list[tuple[str, str]]:
    rows: list[tuple[str, str]] = []
    for raw in stdout.splitlines():
        match = REGISTER_LINE.match(raw)
        if match:
            rows.append((match.group(1), match.group(2)))
    return rows


def native_report(stdout: str) -> dict[str, Any] | None:
    found: dict[str, Any] | None = None
    for raw in reversed(stdout.splitlines()):
        line = raw.strip()
        if not line.startswith("{"):
            continue
        try:
            value = json.loads(line)
        except ValueError:
            continue
        if isinstance(value, dict):
            if "status" in value:
                return value
            found = found or value
    return found


def native_side(report: dict[str, Any] | None, code: int, timed_out: bool, stderr: str) -> Side:
    if timed_out:
        return Side(False, stage="timeout", exit=code, timed_out=True, detail=tail(stderr))
    if report is None:
        return Side(False, stage="error", exit=code, detail=tail(stderr) or "no JSON report")
    registrations = [
        (str(row.get("registry")), str(row.get("id")))
        for row in report.get("registrations") or []
        if isinstance(row, dict)
    ]
    if report.get("status") == "ok":
        return Side(True, registrations, exit=code)
    return Side(
        False,
        registrations,
        stage=str(report.get("stage") or "error"),
        error=str(report.get("error") or ""),
        exit=code,
        detail=tail(stderr),
    )


def classify(native: Side, jvm: Side, compile_error: str | None = None) -> tuple[str, str]:
    if compile_error is not None:
        return "skipped", "compile"
    if native.timed_out or jvm.timed_out:
        return "skipped", "timeout"
    if not jvm.ok:
        # The fixture is supposed to be valid Java; a JVM failure is a harness
        # or generator problem, never a claim about the native runtime.
        return "skipped", "jvm-error"
    if native.ok:
        if native.registrations == jvm.registrations:
            return "ran", "match"
        return "refused", "host-divergence"
    if native.stage in DISCOVERY_STAGES:
        return "needs-loader", native.stage
    return "refused", native.stage or "native-error"


def diff_rows(native: list[tuple[str, str]], jvm: list[tuple[str, str]]) -> dict[str, Any]:
    missing = list((Counter(jvm) - Counter(native)).elements())
    extra = list((Counter(native) - Counter(jvm)).elements())
    first = next(
        (index for index, pair in enumerate(zip(native, jvm)) if pair[0] != pair[1]),
        None,
    )
    return {
        "missing": [list(row) for row in missing],
        "extra": [list(row) for row in extra],
        "first_mismatch": first,
    }


# ---------------------------------------------------------------------------
# Toolchain
# ---------------------------------------------------------------------------


class Toolchain:
    def __init__(self, root: Path, bin_dir: Path | None, build_timeout: float):
        self.root = root
        self.bin_dir = bin_dir
        self.build_timeout = build_timeout
        self.javac = self._jdk_tool("javac")
        self.java = self._jdk_tool("java")
        self.native: Path | None = None
        self.compile_stubs: Path | None = None
        self.runtime_stubs: Path | None = None
        self.reason: str | None = None

    @staticmethod
    def _jdk_tool(name: str) -> str | None:
        home = os.environ.get("JAVA_HOME")
        if home:
            candidate = Path(home) / "bin" / name
            if candidate.is_file():
                return str(candidate)
        return shutil.which(name)

    def prepare(self) -> bool:
        missing = [name for name, tool in (("javac", self.javac), ("java", self.java)) if tool is None]
        if missing:
            self.reason = f"no {', '.join(missing)} on PATH or under JAVA_HOME"
            return False
        if self.bin_dir is not None:
            binary = self.bin_dir / ("horizon-native-java.exe" if os.name == "nt" else "horizon-native-java")
            if not binary.is_file():
                self.reason = f"no horizon-native-java in {self.bin_dir}"
                return False
            self.native = binary
        else:
            built, reason = self._build_native()
            if not built:
                self.reason = reason
                return False
        self.compile_stubs = self._compile_stubs("compile", COMPILE_STUBS)
        self.runtime_stubs = self._compile_stubs("runtime", RUNTIME_STUBS)
        if self.compile_stubs is None or self.runtime_stubs is None:
            self.reason = "javac could not compile the net.minecraft stubs"
            return False
        return True

    def _build_native(self) -> tuple[bool, str | None]:
        if shutil.which("cargo") is None:
            return False, "cargo is not installed"
        code, timed_out, stdout, stderr = run_bounded(
            ["cargo", "build", "--quiet", "-p", "horizon-native-java"],
            self.build_timeout,
            ROOT,
        )
        if timed_out:
            return False, f"cargo build timed out after {self.build_timeout:g}s"
        if code != 0:
            return False, f"cargo build failed: {tail(stderr or stdout)}"
        name = "horizon-native-java.exe" if os.name == "nt" else "horizon-native-java"
        binary = lab.cargo_target() / "debug" / name
        if not binary.is_file():
            return False, f"cargo build left no {binary}"
        self.native = binary
        return True, None

    def _compile_stubs(self, tag: str, sources: dict[str, str]) -> Path | None:
        root = self.root / f"{tag}-stubs"
        source_root = root / "src"
        classes = root / "classes"
        source_root.mkdir(parents=True, exist_ok=True)
        classes.mkdir(parents=True, exist_ok=True)
        paths = []
        for relative, source in sources.items():
            path = source_root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(source.lstrip(), encoding="utf-8")
            paths.append(str(path))
        code, _, _, _ = run_bounded(
            [str(self.javac), "--release", "17", "-d", str(classes), *paths],
            self.build_timeout,
            root,
        )
        return classes if code == 0 else None


def write_jar(path: Path, classes: Path, mod_id: str) -> None:
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        archive.writestr(
            "fabric.mod.json",
            json.dumps(
                {
                    "schemaVersion": 1,
                    "id": mod_id,
                    "version": "1.0.0",
                    "depends": {"minecraft": "26.3"},
                    "entrypoints": {"main": ["demo.Entry"]},
                },
                separators=(",", ":"),
            ),
        )
        for class_file in sorted(classes.rglob("*.class")):
            archive.write(class_file, class_file.relative_to(classes).as_posix())


def run_fixture(
    toolchain: Toolchain,
    work: Path,
    fixture: Fixture,
    probes: list[tuple[str, list[str], str, list[str]]],
    timeout: float,
) -> Observation:
    source = render_fixture(probes)
    if len(source.encode("utf-8")) > MAX_SOURCE_BYTES:
        return Observation(
            fixture.name, fixture.seed, fixture.mutations, "skipped", "source-too-large",
            Side(False, stage="skipped"), Side(False, stage="skipped"),
        )
    fixture_root = work / fixture.name
    source_root = fixture_root / "src"
    classes = fixture_root / "classes"
    source_root.mkdir(parents=True, exist_ok=True)
    classes.mkdir(parents=True, exist_ok=True)
    (source_root / "demo").mkdir(parents=True, exist_ok=True)
    source_path = source_root / "demo" / "Entry.java"
    source_path.write_text(source, encoding="utf-8")

    code, timed_out, stdout, stderr = run_bounded(
        [
            str(toolchain.javac), "--release", "17",
            "-cp", str(toolchain.compile_stubs),
            "-d", str(classes),
            str(source_path),
        ],
        timeout,
        fixture_root,
    )
    if timed_out or code != 0:
        detail = tail((stderr or stdout).replace(str(work), "<work>"))
        return Observation(
            fixture.name, fixture.seed, fixture.mutations, "skipped", "compile",
            Side(False, stage="compile"), Side(False, stage="compile"),
            {"detail": detail},
        )

    jar = fixture_root / f"{fixture.name}.jar"
    write_jar(jar, classes, fixture.name)

    native_code, native_timeout, native_out, native_err = run_bounded(
        [str(toolchain.native), "--json", str(jar), "main"], timeout, fixture_root,
    )
    native = native_side(native_report(native_out), native_code, native_timeout, native_err)

    jvm_code, jvm_timeout, jvm_out, jvm_err = run_bounded(
        [str(toolchain.java), "-cp", f"{toolchain.runtime_stubs}{os.pathsep}{jar}", "demo.Entry"],
        timeout,
        fixture_root,
    )
    jvm = Side(
        not jvm_timeout and jvm_code == 0,
        jvm_registrations(jvm_out),
        stage=None if not jvm_timeout and jvm_code == 0 else "error",
        exit=jvm_code,
        timed_out=jvm_timeout,
        detail=tail(jvm_err),
    )

    status, reason = classify(native, jvm)
    return Observation(
        fixture.name, fixture.seed, fixture.mutations, status, reason, native, jvm,
        diff_rows(native.registrations, jvm.registrations),
    )


# ---------------------------------------------------------------------------
# Score
# ---------------------------------------------------------------------------


def fuzz_score(observations: list[Observation], fixtures: int, seed: int, mutations: int) -> dict[str, Any]:
    counts = Counter(observation.status for observation in observations)
    causes = Counter(
        observation.reason for observation in observations if observation.status != "ran"
    )
    total = len(observations)
    ran = counts["ran"]
    refused = counts["refused"]
    skipped = counts["skipped"]
    needs_loader = counts["needs-loader"]
    return {
        "fixtures": fixtures,
        "observed": total,
        "seed": seed,
        "mutations": mutations,
        "ran": ran,
        "refused": refused,
        "skipped": skipped,
        "needs_loader": needs_loader,
        "match_rate": round(ran / total, 4) if total else 0.0,
        "causes": dict(sorted(causes.items())),
    }


def _count(data: dict[str, Any] | None, *keys: str, default: int = 0) -> int:
    value: Any = data or {}
    for key in keys:
        if not isinstance(value, dict):
            return default
        value = value.get(key)
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def compatibility_score(
    probe: dict[str, Any] | None = None,
    ledger: dict[str, Any] | None = None,
    fuzz: dict[str, Any] | None = None,
    previous: dict[str, Any] | None = None,
    label: str | None = None,
) -> dict[str, Any]:
    """Fold the nightly reports into one ran/refused/skipped/needs-loader view."""
    label = label or date.today().isoformat()
    probe_counts = (probe or {}).get("counts") if isinstance((probe or {}).get("counts"), dict) else {}
    probe_total = _count(probe_counts, "total")

    summary = (ledger or {}).get("summary") if isinstance((ledger or {}).get("summary"), dict) else {}
    recommendations = summary.get("recommendations") if isinstance(summary.get("recommendations"), dict) else {}
    if summary:
        native = {
            "total": _count(summary, "total"),
            "ran": _count(summary, "ran"),
            "refused": _count(summary, "refused"),
            "skipped": _count(summary, "skipped"),
            "needs_loader": _count(recommendations, "loader"),
            "native_gap": _count(recommendations, "native-gap"),
            "native": _count(recommendations, "native"),
        }
    elif probe_counts:
        # No dedicated-server run: the probe's verdicts are the best view left.
        # `fallback` is precisely "recognized but needs the loader or a JVM".
        native = {
            "total": probe_total,
            "ran": _count(probe_counts, "ok"),
            "refused": _count(probe_counts, "unsupported"),
            "skipped": _count(probe_counts, "metadata-only"),
            "needs_loader": _count(probe_counts, "fallback"),
            "native_gap": _count(probe_counts, "unsupported"),
            "native": _count(probe_counts, "ok"),
        }
    else:
        native = {
            "total": 0, "ran": 0, "refused": 0, "skipped": 0,
            "needs_loader": 0, "native_gap": 0, "native": 0,
        }
    native["ran_rate"] = round(native["ran"] / native["total"], 4) if native["total"] else 0.0

    fuzz_raw = (fuzz or {}).get("score") if isinstance((fuzz or {}).get("score"), dict) else {}
    fuzz_score_view = {
        "fixtures": _count(fuzz_raw, "fixtures"),
        "ran": _count(fuzz_raw, "ran"),
        "refused": _count(fuzz_raw, "refused"),
        "skipped": _count(fuzz_raw, "skipped"),
        "needs_loader": _count(fuzz_raw, "needs_loader"),
        "mutations": _count(fuzz_raw, "mutations"),
        "seed": fuzz_raw.get("seed") if isinstance(fuzz_raw, dict) else None,
        "match_rate": fuzz_raw.get("match_rate", 0.0) if isinstance(fuzz_raw, dict) else 0.0,
    }

    overall = native if native["total"] else {
        "total": fuzz_score_view["fixtures"],
        "ran": fuzz_score_view["ran"],
        "refused": fuzz_score_view["refused"],
        "skipped": fuzz_score_view["skipped"],
        "needs_loader": fuzz_score_view["needs_loader"],
        "ran_rate": fuzz_score_view["match_rate"],
    }

    score = {
        "schema": 1,
        "kind": "java-compatibility-score",
        "label": label,
        "sources": {
            "probe": probe is not None,
            "ledger": ledger is not None,
            "fuzz": fuzz is not None,
        },
        "probe": {
            "total": probe_total,
            "ok": _count(probe_counts, "ok"),
            "fallback": _count(probe_counts, "fallback"),
            "unsupported": _count(probe_counts, "unsupported"),
            "metadata_only": _count(probe_counts, "metadata-only"),
        },
        "native": native,
        "fuzz": fuzz_score_view,
        "overall": overall,
    }
    if isinstance(previous, dict) and isinstance(previous.get("overall"), dict):
        old = previous["overall"]
        deltas = {
            key: _count(overall, key) - _count(old, key)
            for key in ("ran", "refused", "skipped", "needs_loader")
        }
        score["trend"] = {
            "previous": previous.get("label"),
            "deltas": deltas,
        }
    return score


def score_markdown(score: dict[str, Any]) -> str:
    overall = score.get("overall") or {}
    total = _count(overall, "total")
    ran = _count(overall, "ran")
    refused = _count(overall, "refused")
    skipped = _count(overall, "skipped")
    needs = _count(overall, "needs_loader")
    rate = overall.get("ran_rate", 0.0)
    lines = [
        "- compatibility: {ran}/{total} ran ({rate:.1%}), {refused} refused, {skipped} skipped, "
        "{needs} needs loader".format(
            ran=ran, total=total, rate=float(rate or 0.0), refused=refused,
            skipped=skipped, needs=needs,
        )
    ]
    trend = score.get("trend")
    if isinstance(trend, dict) and isinstance(trend.get("deltas"), dict):
        deltas = trend["deltas"]
        lines.append(
            "- compatibility trend vs {}: ran {:+d}, refused {:+d}, skipped {:+d}, needs-loader {:+d}".format(
                trend.get("previous") or "previous",
                _count(deltas, "ran"),
                _count(deltas, "refused"),
                _count(deltas, "skipped"),
                _count(deltas, "needs_loader"),
            )
        )
    fuzz = score.get("fuzz") or {}
    fixtures = _count(fuzz, "fixtures")
    if fixtures:
        lines.append(
            "- differential fuzz: {fixtures} fixtures, seed {seed}, {ran} matched, {refused} native "
            "divergences, {skipped} skipped, {mutations} mutations ({rate:.1%} match)".format(
                fixtures=fixtures,
                seed=fuzz.get("seed"),
                ran=_count(fuzz, "ran"),
                refused=_count(fuzz, "refused"),
                skipped=_count(fuzz, "skipped"),
                mutations=_count(fuzz, "mutations"),
                rate=float(fuzz.get("match_rate") or 0.0),
            )
        )
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def run(args: argparse.Namespace) -> int:
    if not 1 <= args.fixtures <= MAX_FIXTURES:
        print(f"java differential fuzz: --fixtures must be 1..{MAX_FIXTURES}", file=sys.stderr)
        return 1
    if args.mutations < 0 or args.mutations > MAX_MUTATIONS:
        print(f"java differential fuzz: --mutations must be 0..{MAX_MUTATIONS}", file=sys.stderr)
        return 1

    work = Path(args.work).resolve() if args.work else Path(tempfile.mkdtemp(prefix="horizon-java-fuzz-"))
    work.mkdir(parents=True, exist_ok=True)
    toolchain = Toolchain(work, Path(args.bin_dir).resolve() if args.bin_dir else None, args.build_timeout)
    try:
        if not toolchain.prepare():
            print(f"java differential fuzz skipped: {toolchain.reason}", file=sys.stderr)
            return 2
        observations = []
        for index in range(args.fixtures):
            fixture, probes = fixture_for(args.seed, index, mutations=args.mutations)
            observation = run_fixture(toolchain, work, fixture, probes, args.timeout)
            observations.append(observation)
            if not args.json:
                print(
                    f"{observation.fixture}: {observation.status} ({observation.reason}) "
                    f"{len(observation.mutations)} mutation(s)",
                    file=sys.stderr,
                )
        score = fuzz_score(
            observations,
            args.fixtures,
            args.seed,
            sum(len(observation.mutations) for observation in observations),
        )
        report = {
            "schema": 1,
            "kind": "java-differential-fuzz",
            "seed": args.seed,
            "fixtures": args.fixtures,
            "score": score,
            "runs": [observation.json() for observation in observations],
        }
        text = json.dumps(report, indent=2, sort_keys=True) + "\n"
        if args.output:
            output = Path(args.output)
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_text(text, encoding="utf-8")
        print(text, end="")
        return 0
    finally:
        if not args.keep and not args.work:
            shutil.rmtree(work, ignore_errors=True)


def load_json(path: str | None) -> dict[str, Any] | None:
    if not path:
        return None
    try:
        value = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    return value if isinstance(value, dict) else None


def score_only(args: argparse.Namespace) -> int:
    score = compatibility_score(
        probe=load_json(args.probe),
        ledger=load_json(args.ledger),
        fuzz=load_json(args.fuzz),
        previous=load_json(args.previous),
        label=args.label,
    )
    if args.output:
        output = Path(args.output)
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(score, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(score_markdown(score))
    return 0


def self_test() -> int:
    """Exercise generation, mutation, parsing and scoring with no JDK or cargo."""
    failures: list[str] = []

    def check(condition: bool, message: str) -> None:
        if not condition:
            failures.append(message)

    first = generate_fixture(DEFAULT_SEED)
    second = generate_fixture(DEFAULT_SEED)
    check(first == second, "same seed must produce the same fixture")
    check(generate_fixture(DEFAULT_SEED + 1).probes != first.probes, "a new seed must change the recipe")
    check(len(first.probes) == 1, "a fixture must hold exactly one probe")
    check(len(first.mutations) <= DEFAULT_MUTATIONS, "mutation budget is not bounded")
    _fixture, rendered = fixture_for(DEFAULT_SEED, 0)
    source = render_fixture(rendered)
    check(source.startswith("package demo;"), "source is not a Java class")
    check("Registry.register" in source, "the fixture never registers a result")
    check(source.count("private static String p") == len(rendered), "probe methods are not one per result")

    native = native_report(json.dumps({"status": "ok", "registrations": [
        {"registry": "minecraft:item", "id": "fuzz:p0=1"},
    ]}))
    parsed_native = native_side(native, 0, False, "")
    parsed_jvm = Side(True, jvm_registrations("noise\n@register\tminecraft:item\tfuzz:p0=1\n"))
    check(classify(parsed_native, parsed_jvm) == ("ran", "match"), "matching rows must be `ran`")
    divergent = Side(True, [("minecraft:item", "fuzz:p0=0")])
    check(classify(parsed_native, divergent) == ("refused", "host-divergence"), "differing rows must be refused")
    refused = Side(False, stage="execute", error="boom")
    check(classify(refused, parsed_jvm) == ("refused", "execute"), "an execute error must be refused")
    missing = Side(False, stage="discover", error="no entrypoint")
    check(classify(missing, parsed_jvm) == ("needs-loader", "discover"), "a discovery error needs the loader")
    check(classify(native_side(None, 0, False, ""), parsed_jvm) == ("refused", "error"),
          "a missing report must be refused")

    probe = {"counts": {"total": 10, "ok": 4, "fallback": 3, "unsupported": 2, "metadata-only": 1}}
    ledger = {
        "summary": {
            "total": 10, "ran": 6, "refused": 3, "skipped": 1,
            "recommendations": {"native": 5, "native-gap": 3, "loader": 2},
        }
    }
    fuzz = {"score": {"fixtures": 4, "ran": 3, "refused": 1, "skipped": 0,
                      "needs_loader": 0, "mutations": 5, "seed": DEFAULT_SEED, "match_rate": 0.75}}
    score = compatibility_score(probe, ledger, fuzz, label="2026-11-06")
    check(score["native"]["ran"] == 6, "ledger ran count was not used")
    check(score["native"]["needs_loader"] == 2, "loader recommendation was not mapped to needs-loader")
    check(score["overall"]["total"] == 10, "overall must follow the ledger when it is present")
    previous = {"label": "2026-11-05", "overall": {"ran": 5, "refused": 4, "skipped": 1, "needs_loader": 2}}
    trend = compatibility_score(probe, ledger, fuzz, previous=previous, label="2026-11-06")["trend"]
    check(trend["deltas"] == {"ran": 1, "refused": -1, "skipped": 0, "needs_loader": 0},
          "trend deltas are wrong")
    markdown = score_markdown(compatibility_score(probe, ledger, fuzz, previous=previous, label="2026-11-06"))
    check("needs loader" in markdown and "compatibility trend" in markdown, "the summary omits the trend")

    if failures:
        for failure in failures:
            print(f"java differential fuzz self-test: FAILED: {failure}", file=sys.stderr)
        return 1
    print("java differential fuzz self-test: ok")
    return 0


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--fixtures", type=int, default=DEFAULT_FIXTURES,
                        help=f"fixtures to run (default {DEFAULT_FIXTURES}, max {MAX_FIXTURES})")
    parser.add_argument("--seed", type=int, default=DEFAULT_SEED,
                        help=f"deterministic generator seed (default {DEFAULT_SEED})")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT,
                        help=f"seconds per compile/run (default {DEFAULT_TIMEOUT:g})")
    parser.add_argument("--build-timeout", type=float, default=DEFAULT_BUILD_TIMEOUT,
                        help=f"seconds for the one native probe build (default {DEFAULT_BUILD_TIMEOUT:g})")
    parser.add_argument("--mutations", type=int, default=DEFAULT_MUTATIONS,
                        help=f"structural mutations per fixture (default {DEFAULT_MUTATIONS}, max {MAX_MUTATIONS})")
    parser.add_argument("--bin-dir", default=None,
                        help="use this folder's prebuilt horizon-native-java instead of cargo build")
    parser.add_argument("--work", default=None,
                        help="keep the scratch tree here instead of a fresh temporary directory")
    parser.add_argument("--keep", action="store_true", help="keep the scratch tree after the run")
    parser.add_argument("--json", action="store_true", help="only the JSON report on stdout")
    parser.add_argument("--output", default=None, help="write the JSON report here as well")
    parser.add_argument("--score-only", action="store_true",
                        help="fold existing nightly reports into a compatibility score")
    parser.add_argument("--probe", default=None, help="real_mod_corpus.py JSON report")
    parser.add_argument("--ledger", default=None, help="java_mod_server_ledger.py JSON report")
    parser.add_argument("--fuzz", default=None, help="this harness's JSON report")
    parser.add_argument("--previous", default=None, help="the previous compatibility-score.json, for the trend")
    parser.add_argument("--label", default=None, help="the score's date label (default today)")
    parser.add_argument("--self-test", action="store_true", help="check the harness itself, no JDK or cargo")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    if args.self_test:
        return self_test()
    if args.score_only:
        return score_only(args)
    return run(args)


if __name__ == "__main__":
    raise SystemExit(main())

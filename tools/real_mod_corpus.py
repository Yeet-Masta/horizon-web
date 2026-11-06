#!/usr/bin/env python3
"""Bounded corpus harness for real Fabric and NeoForge mod jars.

    real_mod_corpus.py --fabric-jars DIR [--fabric-jars DIR]...
                       [--neoforge-jars DIR]... [--server DIR]
                       [--limit N] [--timeout S] [--bin-dir DIR] [--json] [--self-test]
                       [--nested|--no-nested] [--nested-limit N] [--nested-depth N]
                       [--nested-size-cap-mib N]

Reads every jar's loader metadata (`fabric.mod.json`, `META-INF/neoforge.mods.toml` or its older
`META-INF/mods.toml`), then runs one short probe per jar and classifies it:

    ok             the native path ran the Fabric main entrypoint, or FML listed the NeoForge mod
    fallback       the jar is recognized but needs the loader or embedded JVM: a client/server-only
                   entrypoint, an unsupported adapter or bytecode, mixin-only, a nested-jar bundle,
                   or a metadata-level `@Mod` scan when `--list-fml-mods` is unavailable
    unsupported    no loader metadata, no entrypoint and no mixin, no `@Mod` class, or an unreadable jar
    metadata-only  no probe ran (NeoForge without `--server`, or the probe toolchain is unavailable)

Every jar considered for probing is hashed and deduplicated by sha256, within a 1 GiB hashing budget; a
duplicate is reported and skipped. Jars above the size cap are skipped without hashing, and a jar past
the hash budget or the `--limit` is reported as deferred. Probes run `cargo run --quiet` against this
checkout with a per-probe timeout (60 s by default), in their own process group, with captured output
capped at 256 KiB per stream. `--bin-dir` skips the build and runs prebuilt
`horizon-native-java`/`horizon-jvm` binaries instead.

Nested jars (Fabric `META-INF/jars/*.jar`, NeoForge `META-INF/jarjar/*.jar`, plus paths named by the
outer metadata) are extracted into a fresh temporary directory and probed with the same tools, then the
outer verdict folds them in. Extraction is bounded: `--nested-limit` jars per outer (8), 32 in total
across the run, `--nested-depth` levels (1), and `--nested-size-cap-mib` MiB each (16); duplicates are
skipped by sha256. `--no-nested` restores the outer-only report. The temporary directory is removed
after each outer jar, and no game jar or asset directory is touched.

The report (human table, or `--json`) is deterministic for the same roots: jars are ordered by source
and relative path, and reasons carry no timings or process ids.

Exit 0 even when mods fail or nothing is found; exit 1 only when the harness itself cannot run (a named
directory is missing, the server directory is not an installed NeoForge server, or `--limit` is invalid).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import tomllib
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]

DEFAULT_LIMIT = 32
DEFAULT_TIMEOUT = 60.0
DEFAULT_BUILD_TIMEOUT = 900.0
DEFAULT_SIZE_CAP_MIB = 64
DEFAULT_HASH_BUDGET_MIB = 1024
DEFAULT_NESTED_LIMIT = 8
DEFAULT_NESTED_DEPTH = 1
DEFAULT_NESTED_SIZE_CAP_MIB = 16
NESTED_TOTAL_LIMIT = 32
METADATA_CAP = 1 << 20
OUTPUT_CAP = 256 << 10
TAIL = 240

VERDICTS = ("ok", "fallback", "unsupported", "metadata-only")


class HarnessError(Exception):
    """The harness cannot run as asked (bad path, bad limit); not a mod failure."""


class CorpusParser(argparse.ArgumentParser):
    """argparse exits 2 on a bad CLI; this harness reserves 1 for harness errors."""

    def error(self, message: str) -> None:
        self.print_usage(sys.stderr)
        print(f"{self.prog}: error: {message}", file=sys.stderr)
        raise SystemExit(1)


@dataclass(frozen=True)
class JarFile:
    source: str
    root: Path
    file: str
    path: Path
    size: int
    sha256: str


@dataclass
class Probe:
    tool: str
    mode: str
    code: int
    timed_out: bool
    stdout: str
    stderr: str

    @property
    def output(self) -> str:
        return f"{self.stdout}\n{self.stderr}"

    def json(self) -> dict[str, Any] | None:
        """The tool's last JSON object on stdout; mods may print before it."""
        found: dict[str, Any] | None = None
        for raw in reversed(self.stdout.splitlines()):
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

    def status(self) -> str | None:
        value = self.json()
        return value.get("status") if value else None

    def report(self) -> dict[str, Any]:
        report = {
            "tool": self.tool,
            "mode": self.mode,
            "exit": self.code,
            "timed_out": self.timed_out,
            "status": self.status(),
        }
        if self.mode == "mixin-plans":
            value = self.json() or {}
            for key in ("plans", "unresolved", "configs"):
                entries = value.get(key)
                report[key] = len(entries) if isinstance(entries, list) else (
                    0 if value.get("status") == "ok" else "error"
                )
        return report


def tail(probe: Probe) -> str:
    """A short, single-line diagnostic from a failed probe."""
    text = " ".join(probe.output.split())
    return text[-TAIL:] if text else "no output"


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def read_member(archive: zipfile.ZipFile, name: str) -> bytes | None:
    """Read one metadata member, refusing anything past the metadata cap."""
    try:
        with archive.open(name) as member:
            data = member.read(METADATA_CAP + 1)
    except (KeyError, OSError, zipfile.BadZipFile, RuntimeError):
        return None
    if len(data) > METADATA_CAP:
        raise ValueError(f"{name} is larger than {METADATA_CAP} bytes")
    return data


def mixin_name(name: str) -> bool:
    if name.endswith(".mixins.json"):
        return True
    return "/" not in name and name.endswith(".mixin.json")


def nested_member(name: str) -> bool:
    """A jar entry in the Fabric JiJ or NeoForge JarJar directory."""
    return name.endswith(".jar") and (
        name.startswith("META-INF/jars/") or name.startswith("META-INF/jarjar/")
    )


def json_names(value: Any) -> list[str]:
    """Names from a Fabric metadata field that is a string, a list, or an object."""
    if isinstance(value, str):
        return [value]
    if isinstance(value, list):
        names: list[str] = []
        for item in value:
            names.extend(json_names(item))
        return names
    if isinstance(value, dict):
        names = []
        for item in value.values():
            names.extend(json_names(item))
        return names
    return []


def version_string(value: Any) -> str | None:
    if isinstance(value, str):
        return value
    if isinstance(value, dict):
        for key in ("version", "id", "name"):
            if isinstance(value.get(key), str):
                return value[key]
    if isinstance(value, (int, float)):
        return str(value)
    return None


def parse_fabric(data: bytes, root_mixins: list[str], nested: int) -> dict[str, Any]:
    value = json.loads(data.decode("utf-8", "replace"))
    if not isinstance(value, dict):
        raise ValueError("fabric.mod.json is not an object")
    entrypoints: dict[str, int] = {}
    declared = value.get("entrypoints")
    if isinstance(declared, dict):
        for kind, entry in declared.items():
            if isinstance(entry, list):
                entrypoints[kind] = len(entry)
            elif entry is not None:
                entrypoints[kind] = 1
    mixins = set(json_names(value.get("mixins")))
    mixins.update(root_mixins)
    jars = json_names(value.get("jars"))
    nested_refs = sorted({ref for ref in jars if ref.endswith(".jar")})
    depends = value.get("depends")
    minecraft = None
    if isinstance(depends, dict):
        minecraft = version_string(depends.get("minecraft"))
    mod_id = version_string(value.get("id"))
    return {
        "loader": "fabric",
        "id": mod_id,
        "version": version_string(value.get("version")),
        "name": version_string(value.get("name")),
        "mod_ids": [mod_id] if mod_id else [],
        "minecraft": minecraft,
        "environment": version_string(value.get("environment")),
        "entrypoints": sum(entrypoints.values()),
        "entrypoint_kinds": sorted(entrypoints),
        "mixins": bool(mixins),
        "mixin_configs": len(mixins),
        "jij": bool(jars or nested),
        "jij_jars": nested,
        "jij_declared": len(jars),
        "nested_refs": nested_refs,
        "metadata": "fabric.mod.json",
    }


def hand_parse_neo(text: str) -> dict[str, Any]:
    """A last-resort reader for a `mods.toml` tomllib rejects."""
    mod_loader = None
    section = ""
    mods: list[dict[str, str]] = []
    current: dict[str, str] = {}
    mixins: list[dict[str, str]] = []
    for raw in text.splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        if line.startswith("["):
            if section == "[[mods]]" and current:
                mods.append(current)
                current = {}
            section = line
            continue
        key, sep, value = line.partition("=")
        if not sep:
            continue
        key = key.strip()
        value = value.strip().strip('"\'')
        if section == "[[mods]]":
            current[key] = value
        elif section == "[[mixins]]" and key == "config":
            mixins.append({"config": value})
        elif not section and key == "modLoader":
            mod_loader = value
    if current:
        mods.append(current)
    return {"modLoader": mod_loader, "mods": mods, "mixins": mixins}


def parse_neoforge(data: bytes, root_mixins: list[str], nested: int) -> dict[str, Any]:
    text = data.decode("utf-8", "replace")
    try:
        value = tomllib.loads(text)
    except tomllib.TOMLDecodeError:
        value = hand_parse_neo(text)
    if not isinstance(value, dict):
        raise ValueError("mods.toml is not a table")
    mods = [entry for entry in value.get("mods") or [] if isinstance(entry, dict)]
    first = mods[0] if mods else {}
    mod_ids = [str(entry["modId"]) for entry in mods if entry.get("modId")]
    mixins = set(root_mixins)
    for entry in value.get("mixins") or []:
        if isinstance(entry, dict) and isinstance(entry.get("config"), str):
            mixins.add(entry["config"])
        elif isinstance(entry, str):
            mixins.add(entry)
    dependencies = value.get("dependencies")
    minecraft = None
    if isinstance(dependencies, dict):
        for mod_id, entries in dependencies.items():
            if not isinstance(entries, list):
                continue
            for entry in entries:
                if not isinstance(entry, dict):
                    continue
                if mod_id == "minecraft" or entry.get("modId") == "minecraft":
                    minecraft = version_string(entry.get("versionRange") or entry.get("version"))
                    break
            if minecraft:
                break
    jij = value.get("jarJar") or value.get("jars") or []
    nested_refs = sorted({ref for ref in json_names(jij) if ref.endswith(".jar")})
    return {
        "loader": "neoforge",
        "id": version_string(first.get("modId")),
        "version": version_string(first.get("version")),
        "name": version_string(first.get("displayName") or first.get("modId")),
        "mod_ids": mod_ids,
        "minecraft": minecraft,
        "environment": None,
        "entrypoints": 0,
        "entrypoint_kinds": [],
        "mixins": bool(mixins),
        "mixin_configs": len(mixins),
        "jij": bool(jij or nested),
        "jij_jars": nested,
        "jij_declared": len(jij) if isinstance(jij, list) else 0,
        "nested_refs": nested_refs,
        "metadata": "META-INF/neoforge.mods.toml",
    }


def unknown_metadata() -> dict[str, Any]:
    return {
        "loader": "unknown",
        "id": None,
        "version": None,
        "name": None,
        "mod_ids": [],
        "minecraft": None,
        "environment": None,
        "entrypoints": 0,
        "entrypoint_kinds": [],
        "mixins": False,
        "mixin_configs": 0,
        "jij": False,
        "jij_jars": 0,
        "jij_declared": 0,
        "nested_refs": [],
        "metadata": None,
    }


def read_metadata(path: Path, source: str) -> tuple[dict[str, Any], str | None]:
    """Loader metadata for one jar, or `(unknown, reason)` when it has none or is unreadable."""
    try:
        with zipfile.ZipFile(path) as archive:
            names = archive.namelist()
            fabric_data = read_member(archive, "fabric.mod.json") if "fabric.mod.json" in names else None
            neo_name = next(
                (name for name in ("META-INF/neoforge.mods.toml", "META-INF/mods.toml") if name in names),
                None,
            )
            neo_data = read_member(archive, neo_name) if neo_name else None
            root_mixins = sorted({name for name in names if mixin_name(name)})
            all_entries = set(names)
            nested_entries = {name for name in all_entries if nested_member(name)}
            nested_fabric = sum(1 for name in nested_entries if name.startswith("META-INF/jars/"))
            nested_neo = sum(1 for name in nested_entries if name.startswith("META-INF/jarjar/"))
    except (zipfile.BadZipFile, OSError, ValueError) as error:
        return unknown_metadata(), f"not a readable jar: {error}"

    # The directory says which loader to prefer when a jar ships metadata for both (a multi-loader jar).
    choices = []
    if fabric_data is not None:
        choices.append(("fabric", fabric_data, nested_fabric))
    if neo_data is not None:
        choices.append(("neoforge", neo_data, nested_neo))
    if not choices:
        return unknown_metadata(), "no fabric.mod.json or neoforge.mods.toml"
    preferred = next((choice for choice in choices if choice[0] == source), choices[0])
    loader, data, nested = preferred
    try:
        if loader == "fabric":
            meta = parse_fabric(data, root_mixins, nested)
        else:
            meta = parse_neoforge(data, root_mixins, nested)
    except (ValueError, UnicodeError, TypeError) as error:
        return unknown_metadata(), f"invalid {loader} metadata: {error}"
    # Nested jars live in the well-known directories; metadata may also name a jar explicitly.
    named = {ref.lstrip("/") for ref in meta.get("nested_refs") or [] if ref.lstrip("/") in all_entries}
    meta["nested_refs"] = sorted(nested_entries | named)
    meta["jij"] = meta["jij"] or bool(meta["nested_refs"])
    return meta, None


def drain(stream: Any, sink: list[bytes], cap: int) -> None:
    """Read a child stream to EOF, keeping at most `cap` bytes."""
    kept = bytearray()
    while True:
        chunk = stream.read(65536)
        if not chunk:
            break
        if len(kept) < cap:
            kept.extend(chunk[: cap - len(kept)])
    sink.append(bytes(kept))


def run_command(command: list[str], timeout: float) -> tuple[int, bool, str, str]:
    try:
        proc = subprocess.Popen(
            command,
            cwd=ROOT,
            env=dict(os.environ, CARGO_TERM_COLOR="never"),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
    except OSError as error:
        return -1, False, "", str(error)
    stdout: list[bytes] = []
    stderr: list[bytes] = []
    readers = [
        threading.Thread(target=drain, args=(proc.stdout, stdout, OUTPUT_CAP), daemon=True),
        threading.Thread(target=drain, args=(proc.stderr, stderr, OUTPUT_CAP), daemon=True),
    ]
    for reader in readers:
        reader.start()
    timed_out = False
    try:
        code = proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        code = proc.wait()
    for reader in readers:
        reader.join(timeout=5)
    if proc.stdout is not None:
        proc.stdout.close()
    if proc.stderr is not None:
        proc.stderr.close()
    return (
        code,
        timed_out,
        b"".join(stdout).decode("utf-8", "replace"),
        b"".join(stderr).decode("utf-8", "replace"),
    )


def run_probe(tool: str, mode: str, args: list[str], timeout: float,
              bin_dir: Path | None = None) -> Probe:
    if bin_dir is not None:
        command = [str(bin_dir / tool), *args]
    else:
        command = [
            "cargo",
            "run",
            "--quiet",
            "--manifest-path",
            str(ROOT / "Cargo.toml"),
            "-p",
            tool,
            "--",
            *args,
        ]
    code, timed_out, stdout, stderr = run_command(command, timeout)
    return Probe(tool, mode, code, timed_out, stdout, stderr)


def build_probes(timeout: float, bin_dir: Path | None) -> tuple[bool, str | None]:
    if bin_dir is not None:
        missing = [
            tool for tool in ("horizon-native-java", "horizon-jvm")
            if not (bin_dir / tool).is_file()
        ]
        if missing:
            return False, f"missing prebuilt binary/binaries in {bin_dir}: {', '.join(missing)}"
        return True, None
    if shutil.which("cargo") is None:
        return False, "cargo is not installed"
    code, timed_out, stdout, stderr = run_command(
        [
            "cargo",
            "build",
            "--quiet",
            "--manifest-path",
            str(ROOT / "Cargo.toml"),
            "-p",
            "horizon-native-java",
            "-p",
            "horizon-jvm",
        ],
        timeout,
    )
    if timed_out:
        return False, f"cargo build timed out after {timeout:g}s"
    if code != 0:
        detail = (stderr or stdout).strip().replace("\n", " ")[-TAIL:]
        return False, f"cargo build failed (exit {code}): {detail or 'no output'}"
    return True, None


class ProbeToolchain:
    """The probe binaries plus the one upfront build. Tests inject a stand-in, never a subprocess."""

    def __init__(self, timeout: float, build_timeout: float, bin_dir: Path | None = None):
        self.timeout = timeout
        self.build_timeout = build_timeout
        self.bin_dir = bin_dir

    def probe(self, tool: str, mode: str, args: list[str]) -> Probe:
        return run_probe(tool, mode, args, self.timeout, self.bin_dir)

    def build(self) -> tuple[bool, str | None]:
        return build_probes(self.build_timeout, self.bin_dir)


class NestedState:
    """The bounded nested-jar budget plus the toolchain to probe with, shared across outer jars."""

    def __init__(self, args: argparse.Namespace, toolchain: ProbeToolchain, server: Path | None,
                 available: bool, reason: str | None, selected: list[JarFile]):
        self.toolchain = toolchain
        self.server = server
        self.available = available
        self.reason = reason
        self.limit = args.nested_limit
        self.total = NESTED_TOTAL_LIMIT
        self.depth = args.nested_depth
        self.size_cap = args.nested_size_cap_mib << 20
        self.seen: dict[str, str] = {jar.sha256: jar.file for jar in selected}
        self.used = 0
        self.outer_used = 0

    def begin_outer(self) -> None:
        self.outer_used = 0


def list_entrypoints(probe: Probe) -> dict[str, int]:
    """`kind<TAB>entrypoint` lines from `horizon-jvm --list-entrypoints`."""
    kinds: dict[str, int] = {}
    for line in probe.stdout.splitlines():
        kind, sep, value = line.partition("\t")
        if sep and kind and value.strip():
            kinds[kind] = kinds.get(kind, 0) + 1
    return kinds


def describe_kinds(kinds: dict[str, int]) -> str:
    return ", ".join(f"{kind} ({count})" for kind, count in sorted(kinds.items()))


def retry_server_kind(jar: Path, kinds: dict[str, int],
                      toolchain: ProbeToolchain) -> tuple[Probe | None, str | None]:
    """Run the native path for a server-only Fabric jar, the one non-client kind worth executing."""
    if "server" not in kinds or "main" in kinds:
        return None, None
    probe = toolchain.probe("horizon-native-java", "server", ["--json", str(jar), "server"])
    value = probe.json()
    if probe.code == 0 and value and value.get("status") == "ok":
        entrypoints = len(value.get("entrypoints") or [])
        host_calls = value.get("host_calls", 0)
        return probe, f"native server entrypoint ran ({entrypoints} entrypoint(s), {host_calls} host call(s))"
    return probe, None


def probe_fabric(jar: Path, meta: dict[str, Any], available: bool, reason: str | None,
                 toolchain: ProbeToolchain) -> tuple[str, str, list[Probe]]:
    if not available:
        return "metadata-only", f"probe unavailable: {reason}", []
    probes: list[Probe] = []
    main = toolchain.probe("horizon-native-java", "main", ["--json", str(jar), "main"])
    probes.append(main)
    value = main.json()
    if main.code == 0 and value and value.get("status") == "ok":
        entrypoints = len(value.get("entrypoints") or [])
        host_calls = value.get("host_calls", 0)
        mixins = value.get("mixin_configs", 0)
        return (
            "ok",
            f"native main entrypoint ran ({entrypoints} entrypoint(s), {host_calls} host call(s); "
            f"{mixins} mixin config(s) not applied)",
            probes,
        )
    if main.timed_out:
        return "fallback", f"native main probe timed out after {toolchain.timeout:g}s; embedded JVM fallback", probes
    if value and value.get("status") == "error":
        stage = value.get("stage")
        error = value.get("error", "unknown error")
        if stage == "load":
            return "unsupported", f"cannot load jar: {error}", probes
        if stage == "execute":
            return "fallback", f"native execution stopped: {error}; embedded JVM fallback", probes
    # No main entrypoint (stage `discover`), or an unparsed native failure: planning and listing are
    # still meaningful, and they say whether the jar has another kind or only mixins.
    plans = toolchain.probe("horizon-native-java", "mixin-plans", ["--mixin-plans", str(jar)])
    probes.append(plans)
    plan_value = plans.json() or {}
    plan_count = len(plan_value.get("plans") or [])
    config_count = len(plan_value.get("configs") or [])
    listing = toolchain.probe("horizon-jvm", "list-entrypoints", ["--list-entrypoints", str(jar)])
    probes.append(listing)
    kinds = list_entrypoints(listing)
    if kinds:
        retry, retry_reason = retry_server_kind(jar, kinds, toolchain)
        if retry is not None:
            probes.append(retry)
        if retry_reason:
            return "ok", retry_reason, probes
        return (
            "fallback",
            f"no main entrypoint; declared kinds: {describe_kinds(kinds)}; needs the loader or embedded JVM",
            probes,
        )
    if plan_count or config_count or meta.get("mixin_configs", 0):
        count = plan_count or config_count or meta.get("mixin_configs", 0)
        noun = "mixin plan" if plan_count else "mixin config"
        return "fallback", f"no entrypoint; {count} {noun}(s) need the loader", probes
    jij = meta.get("jij_jars", 0) or meta.get("jij_declared", 0)
    if jij:
        return (
            "fallback",
            f"bundle of {jij} nested jar(s) with no outer entrypoint; probe the nested jars",
            probes,
        )
    detail = tail(main) if main.code != 0 else "no main entrypoint"
    return "unsupported", f"no main entrypoint and no mixin: {detail}", probes


def fml_mods(probe: Probe) -> list[str]:
    """Mod ids from an `--list-fml-mods` listing, whichever whitespace/JSON shape it uses."""
    value = probe.json()
    if isinstance(value, dict):
        for key in ("mods", "modIds", "ids"):
            entries = value.get(key)
            if isinstance(entries, list):
                return [str(entry) for entry in entries if entry]
            if isinstance(entries, dict):
                return [str(key) for key in entries]
    if isinstance(value, list):
        return [str(entry) for entry in value if entry]
    mods: list[str] = []
    for line in probe.stdout.splitlines():
        text = line.strip()
        if not text or text.startswith("#") or text.startswith("mods="):
            continue
        if text.startswith("mod="):
            text = text[len("mod="):].strip()
        elif "=" in text:
            text = text.split("=", 1)[1].strip()
        text = text.split("\t", 1)[0].strip()
        if text:
            mods.append(text)
    return mods


def flag_rejected(probe: Probe) -> bool:
    """Whether this `horizon-jvm` treats `--list-fml-mods` as a jar path instead of a mode."""
    text = f"{probe.stdout}\n{probe.stderr}".lower()
    return probe.code != 0 and ("--list-fml-mods" in text or "usage:" in text)


def probe_neoforge(jar: Path, meta: dict[str, Any], server: Path | None, available: bool,
                   reason: str | None, toolchain: ProbeToolchain
                   ) -> tuple[str, str, list[Probe]]:
    mod_ids = ", ".join(meta.get("mod_ids") or []) or "none"
    if server is None:
        return "metadata-only", f"NeoForge server not supplied; metadata lists mod id(s): {mod_ids}", []
    if not available:
        return "metadata-only", f"probe unavailable: {reason}", []
    probes: list[Probe] = []
    listing = toolchain.probe(
        "horizon-jvm",
        "list-fml-mods",
        ["--list-fml-mods", "--libs", str(server / "libraries"), str(jar)],
    )
    probes.append(listing)
    if listing.code == 0:
        mods = fml_mods(listing)
        own_ids = set(meta.get("mod_ids") or [])
        if not own_ids:
            own_ids = {mod for mod in mods if mod not in ("minecraft", "neoforge")}
        own = [mod for mod in mods if mod in own_ids]
        if own:
            return (
                "ok",
                f"FML listed the jar's {len(own)} mod(s): {', '.join(own)} ({len(mods)} total)",
                probes,
            )
        if mods:
            return (
                "unsupported",
                f"FML listed {len(mods)} mod(s) but not the jar's {mod_ids}: {', '.join(mods)}",
                probes,
            )
        return "unsupported", "FML listed no mods", probes
    if not flag_rejected(listing):
        return "fallback", f"FML listing failed (exit {listing.code}): {tail(listing)}", probes
    # This checkout's horizon-jvm has no `--list-fml-mods` yet; the bounded @Mod scan still answers
    # whether the jar carries a class FML would construct.
    scan = toolchain.probe("horizon-jvm", "list-entrypoints", ["--list-entrypoints", str(jar)])
    probes.append(scan)
    kinds = list_entrypoints(scan)
    if kinds.get("neoforge"):
        return (
            "fallback",
            f"horizon-jvm has no --list-fml-mods; @Mod scan found {kinds['neoforge']} class(es)",
            probes,
        )
    if scan.code == 0 and not kinds:
        return "unsupported", "horizon-jvm has no --list-fml-mods; no @Mod class in the jar", probes
    return "fallback", f"horizon-jvm has no --list-fml-mods; @Mod scan failed: {tail(scan)}", probes


def collect_jars(roots: list[tuple[str, Path]], limit: int, size_cap: int, hash_budget: int,
                 ) -> tuple[list[JarFile], list[dict[str, Any]], list[dict[str, Any]], list[dict[str, Any]]]:
    candidates: list[tuple[str, Path, str, Path]] = []
    for source, root in roots:
        for path in sorted(root.rglob("*.jar")):
            try:
                file = path.relative_to(root).as_posix()
            except ValueError:
                file = path.name
            candidates.append((source, root, file, path))
    candidates.sort(key=lambda item: (item[0], str(item[1]), item[2]))

    unique: list[JarFile] = []
    skipped: list[dict[str, Any]] = []
    duplicates: list[dict[str, Any]] = []
    deferred: list[dict[str, Any]] = []
    seen: dict[str, JarFile] = {}
    hashed = 0
    budget_hit = False
    for source, root, file, path in candidates:
        try:
            size = path.stat().st_size
        except OSError as error:
            skipped.append({"source": source, "root": str(root), "file": file,
                            "reason": f"cannot stat: {error}"})
            continue
        if size > size_cap:
            skipped.append({"source": source, "root": str(root), "file": file,
                            "reason": f"{size} bytes is past the {size_cap}-byte cap"})
            continue
        if budget_hit or hashed + size > hash_budget:
            budget_hit = True
            deferred.append({"source": source, "root": str(root), "file": file,
                             "reason": "hash budget"})
            continue
        try:
            digest = sha256_of(path)
        except OSError as error:
            skipped.append({"source": source, "root": str(root), "file": file,
                            "reason": f"cannot hash: {error}"})
            continue
        hashed += size
        if digest in seen:
            first = seen[digest]
            duplicates.append({
                "source": source,
                "root": str(root),
                "file": file,
                "same_as": first.file,
                "sha256": digest,
            })
            continue
        jar = JarFile(source, root, file, path, size, digest)
        seen[digest] = jar
        unique.append(jar)
    selected = unique[:limit]
    deferred.extend(
        {"source": jar.source, "root": str(jar.root), "file": jar.file, "reason": "limit"}
        for jar in unique[limit:]
    )
    deferred.sort(key=lambda item: (item["source"], item["root"], item["file"]))
    return selected, skipped, duplicates, deferred


def scrub_paths(text: str, target: Path, name: str, temp: Path) -> str:
    """Keep a nested probe reason free of the random temporary directory."""
    return text.replace(str(target), name).replace(str(temp), "<nested>")


def classify_jar(jar: JarFile, server: Path | None, toolchain: ProbeToolchain, available: bool,
                 reason: str | None) -> tuple[dict[str, Any], dict[str, Any]]:
    meta, error = read_metadata(jar.path, jar.source)
    row: dict[str, Any] = {
        "file": jar.file,
        "root": str(jar.root),
        "source": jar.source,
        "sha256": jar.sha256,
        "size": jar.size,
        "loader": meta["loader"],
        "id": meta["id"],
        "version": meta["version"],
        "name": meta["name"],
        "minecraft": meta["minecraft"],
        "environment": meta["environment"],
        "entrypoints": meta["entrypoints"],
        "entrypoint_kinds": meta["entrypoint_kinds"],
        "mixins": meta["mixins"],
        "mixin_configs": meta["mixin_configs"],
        "jij": meta["jij"],
        "jij_jars": meta["jij_jars"],
        "metadata": meta["metadata"],
    }
    probes: list[Probe] = []
    if error is not None:
        verdict, why = "unsupported", error
    elif meta["loader"] == "fabric":
        verdict, why, probes = probe_fabric(jar.path, meta, available, reason, toolchain)
    elif meta["loader"] == "neoforge":
        verdict, why, probes = probe_neoforge(
            jar.path, meta, server, available, reason, toolchain
        )
    else:
        verdict, why = "unsupported", "no loader metadata"
    row["verdict"] = verdict
    row["reason"] = why
    row["probes"] = [probe.report() for probe in probes]
    for probe in probes:
        if probe.mode == "mixin-plans":
            plan_report = probe.report()
            for key in ("plans", "unresolved", "configs"):
                row[key] = plan_report[key]
            break
    return row, meta


def nested_summary(rows: list[dict[str, Any]], skipped: list[dict[str, Any]]) -> str:
    counts = {verdict: 0 for verdict in VERDICTS}
    for row in rows:
        counts[row["verdict"]] += 1
    text = ", ".join(f"{counts[verdict]} {verdict}" for verdict in VERDICTS if counts[verdict])
    return f"{len(rows)} nested jar(s): {text or 'none readable'}; {len(skipped)} skipped"


def fold_nested(row: dict[str, Any], rows: list[dict[str, Any]],
                skipped: list[dict[str, Any]]) -> None:
    """An outer jar with any successful nested jar is ok; readable nested jars otherwise fallback."""
    if not rows:
        if skipped and row["verdict"] == "fallback":
            row["reason"] = f"{row['reason']}; {len(skipped)} nested jar(s) skipped"
        return
    counts = {verdict: 0 for verdict in VERDICTS}
    for nested in rows:
        counts[nested["verdict"]] += 1
    summary = nested_summary(rows, skipped)
    if counts["ok"]:
        if row["verdict"] == "ok":
            row["reason"] = f"{row['reason']}; nested: {summary}"
        else:
            row["reason"] = f"nested jar(s) ran: {summary}"
        row["verdict"] = "ok"
    elif any(nested["loader"] != "unknown" for nested in rows):
        row["verdict"] = "fallback"
        row["reason"] = f"nested jars probed, none ok ({summary})"


def probe_nested(parent: Path, meta: dict[str, Any], depth: int,
                 state: NestedState) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """Extract and probe `meta`'s nested jars, recursively up to `state.depth`, inside a temp dir."""
    rows: list[dict[str, Any]] = []
    skipped: list[dict[str, Any]] = []
    candidates = meta.get("nested_refs") or []
    if depth > state.depth or not candidates:
        return rows, skipped
    try:
        archive = zipfile.ZipFile(parent)
    except (zipfile.BadZipFile, OSError):
        return rows, skipped
    with archive, tempfile.TemporaryDirectory(prefix="horizon-nested-") as tmp:
        temp = Path(tmp)
        infos = {info.filename: info for info in archive.infolist()}
        for name in candidates:
            if state.outer_used >= state.limit:
                skipped.append({"name": name, "depth": depth,
                                "reason": f"per-jar nested limit {state.limit}"})
                break
            if state.used >= state.total:
                skipped.append({"name": name, "depth": depth,
                                "reason": f"total nested limit {state.total}"})
                break
            info = infos.get(name)
            if info is None:
                continue
            state.outer_used += 1
            state.used += 1
            try:
                with archive.open(info) as member:
                    data = member.read(state.size_cap + 1)
            except (KeyError, OSError, RuntimeError, zipfile.BadZipFile) as error:
                skipped.append({"name": name, "depth": depth,
                                "reason": f"cannot read nested jar: {error}"})
                continue
            if len(data) > state.size_cap:
                skipped.append({"name": name, "depth": depth,
                                "reason": f"{len(data)}+ bytes is past the "
                                          f"{state.size_cap}-byte nested cap"})
                continue
            digest = hashlib.sha256(data).hexdigest()
            if digest in state.seen:
                skipped.append({"name": name, "depth": depth,
                                "reason": f"duplicate of {state.seen[digest]} "
                                          f"(sha256 {digest[:12]})"})
                continue
            state.seen[digest] = name
            target = temp / f"nested-{depth}-{state.used:03d}.jar"
            target.write_bytes(data)
            source = meta["loader"] if meta["loader"] in ("fabric", "neoforge") else "nested"
            nested = JarFile(source, temp, name, target, len(data), digest)
            row, child_meta = classify_jar(
                nested, state.server, state.toolchain, state.available, state.reason
            )
            reason = scrub_paths(row["reason"], target, name, temp)
            rows.append({
                "name": name,
                "loader": row["loader"],
                "id": row["id"],
                "version": row["version"],
                "verdict": row["verdict"],
                "reason": reason,
                "depth": depth,
            })
            for key in ("plans", "unresolved", "configs"):
                if key in row:
                    rows[-1][key] = row[key]
            if depth < state.depth:
                child_rows, child_skipped = probe_nested(target, child_meta, depth + 1, state)
                rows.extend(child_rows)
                skipped.extend(child_skipped)
    return rows, skipped


def classify(jar: JarFile, server: Path | None, toolchain: ProbeToolchain, available: bool,
             reason: str | None, nested: NestedState | None = None) -> dict[str, Any]:
    row, meta = classify_jar(jar, server, toolchain, available, reason)
    if nested is not None:
        nested.begin_outer()
        rows, skipped = probe_nested(jar.path, meta, 1, nested)
        row["nested"] = rows
        row["nested_skipped"] = skipped
        fold_nested(row, rows, skipped)
    return row


def is_installed_server(path: Path) -> bool:
    return (path / "libraries" / "net" / "neoforged").is_dir() and any(
        path.glob("libraries/net/neoforged/neoforge/*/unix_args.txt")
    )


def build_report(args: argparse.Namespace,
                 toolchain: ProbeToolchain | None = None) -> dict[str, Any]:
    limits = {
        "jars": args.limit,
        "size_cap_bytes": args.size_cap_mib << 20,
        "hash_budget_bytes": args.hash_budget_mib << 20,
        "probe_timeout_s": args.timeout,
        "build_timeout_s": args.build_timeout,
    }
    if args.nested:
        limits["nested"] = {
            "per_jar": args.nested_limit,
            "total": NESTED_TOTAL_LIMIT,
            "depth": args.nested_depth,
            "size_cap_bytes": args.nested_size_cap_mib << 20,
        }
    if args.server is not None and not is_installed_server(args.server):
        raise HarnessError(f"{args.server} is not an installed NeoForge server (no unix_args.txt under libraries)")
    roots: list[tuple[str, Path]] = [
        ("fabric", path) for path in args.fabric_jars
    ] + [("neoforge", path) for path in args.neoforge_jars]
    if not roots:
        return {
            "schema": 1,
            "skipped": "no --fabric-jars or --neoforge-jars directory",
            "limits": limits,
            "roots": {"fabric": [], "neoforge": [], "server": None},
            "counts": {verdict: 0 for verdict in VERDICTS} | {"total": 0},
            "jars": [],
            "skipped_jars": [],
            "duplicates": [],
            "deferred": [],
        }
    selected, skipped, duplicates, deferred = collect_jars(
        roots, args.limit, args.size_cap_mib << 20, args.hash_budget_mib << 20
    )
    report: dict[str, Any] = {
        "schema": 1,
        "limits": limits,
        "roots": {
            "fabric": [str(path) for path in args.fabric_jars],
            "neoforge": [str(path) for path in args.neoforge_jars],
            "server": str(args.server) if args.server else None,
        },
        "probe_toolchain": {
            "mode": "prebuilt" if args.bin_dir else "cargo",
            "bin_dir": str(args.bin_dir) if args.bin_dir else None,
            "cargo": shutil.which("cargo"),
            "build": None,
        },
        "counts": {},
        "jars": [],
        "skipped_jars": skipped,
        "duplicates": duplicates,
        "deferred": deferred,
    }
    if not selected:
        if skipped or deferred:
            report["skipped"] = (
                f"no jar left to probe: {len(skipped)} skipped, {len(deferred)} deferred"
            )
        else:
            report["skipped"] = "no jars under the given directories"
        report["counts"] = {verdict: 0 for verdict in VERDICTS} | {"total": 0}
        return report

    if toolchain is None:
        toolchain = ProbeToolchain(args.timeout, args.build_timeout, args.bin_dir)
    available, reason = toolchain.build()
    report["probe_toolchain"]["build"] = "ok" if available else reason
    nested = None
    if args.nested:
        nested = NestedState(args, toolchain, args.server, available, reason, selected)
    rows = [
        classify(jar, args.server, toolchain, available, reason, nested)
        for jar in selected
    ]

    counts = {verdict: 0 for verdict in VERDICTS}
    for row in rows:
        counts[row["verdict"]] += 1
    counts["total"] = len(rows)
    report["counts"] = counts
    report["jars"] = rows
    return report


def print_human(report: dict[str, Any]) -> None:
    if "skipped" in report:
        print(f"real mod corpus: skipped ({report['skipped']})")
        return
    counts = report["counts"]
    print(
        f"real mod corpus: {counts['total']} jars, {counts['ok']} ok, "
        f"{counts['fallback']} fallback, {counts['unsupported']} unsupported"
    )
    extras = [
        f"metadata-only {counts['metadata-only']}",
        f"skipped {len(report['skipped_jars'])}",
        f"duplicates {len(report['duplicates'])}",
        f"deferred {len(report['deferred'])}",
    ]
    print("  " + ", ".join(extras) + " (flags: M mixins, J nested jars)")
    for row in report["jars"]:
        identity = row["id"] or "-"
        version = row["version"] or "-"
        flags = ("M" if row["mixins"] else "-") + ("J" if row["jij"] else "-")
        reason = row["reason"]
        if len(reason) > 240:
            reason = reason[:237] + "..."
        print(
            f"  {row['verdict']:<14} {flags} {row['file']}  "
            f"{identity} {version}  {reason}"
        )
        for nested in row.get("nested") or []:
            why = nested["reason"]
            if len(why) > 160:
                why = why[:157] + "..."
            print(
                f"      nested d{nested['depth']} {nested['verdict']:<14} {nested['name']}  "
                f"{nested['id'] or '-'} {nested['version'] or '-'}  {why}"
            )
        for item in row.get("nested_skipped") or []:
            print(f"      nested-skip d{item['depth']} {item['name']}: {item['reason']}")
    for item in report["skipped_jars"]:
        print(f"  skipped        {item['source']}/{item['file']}: {item['reason']}")
    for item in report["duplicates"]:
        print(f"  duplicate      {item['source']}/{item['file']} = {item['same_as']}")
    for item in report["deferred"]:
        print(f"  deferred       {item['source']}/{item['file']} ({item['reason']})")


def self_test() -> int:
    """Probe a synthetic corpus through the real pipeline, with no JVM, cargo or real jars."""
    def jar(path: Path, members: dict[str, str | bytes]) -> None:
        with zipfile.ZipFile(path, "w") as archive:
            for name, data in members.items():
                archive.writestr(name, data)

    class Stub:
        timeout = 1.0

        def build(self) -> tuple[bool, str | None]:
            return True, None

        def probe(self, tool: str, mode: str, args: list[str]) -> Probe:
            located = next((arg for arg in args if arg.endswith(".jar")), "")
            meta, _ = read_metadata(Path(located), "fabric")
            kinds = meta["entrypoint_kinds"]
            if mode == "list-entrypoints":
                listed = "".join(f"{kind}\tcom.example.Entry\n" for kind in kinds)
                return Probe(tool, mode, 0, False, listed, "")
            if mode == "mixin-plans":
                return Probe(tool, mode, 0, False, '{"plans": [], "configs": []}', "")
            if "main" in kinds:
                body = {"status": "ok", "entrypoints": kinds, "host_calls": 1, "mixin_configs": 0}
                return Probe(tool, mode, 0, False, json.dumps(body), "")
            body = {"status": "error", "stage": "discover", "error": "no main entrypoint"}
            return Probe(tool, mode, 0, False, json.dumps(body), "")

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        jar(root / "demo-ok.jar", {"fabric.mod.json": json.dumps({
            "schemaVersion": 1, "id": "demo", "version": "1.0",
            "entrypoints": {"main": ["com.example.Demo"]},
        })})
        jar(root / "demo-client.jar", {"fabric.mod.json": json.dumps({
            "schemaVersion": 1, "id": "demo-client", "version": "1.0",
            "entrypoints": {"client": ["com.example.Client"]},
        })})
        jar(root / "plain.jar", {"demo/Thing.class": b"\xca\xfe\xba\xbe"})
        shutil.copyfile(root / "demo-ok.jar", root / "demo-ok-copy.jar")
        args = parse_args(["--fabric-jars", str(root)])
        args.fabric_jars = [Path(path).resolve() for path in args.fabric_jars]
        report = build_report(args, Stub())
        expected = {"ok": 1, "fallback": 1, "unsupported": 1, "metadata-only": 0, "total": 3}
        if report["counts"] != expected or len(report["duplicates"]) != 1:
            print(f"real mod corpus self-test: FAILED ({report['counts']}, "
                  f"{len(report['duplicates'])} duplicate(s))", file=sys.stderr)
            return 1
    print("real mod corpus self-test: ok")
    return 0


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = CorpusParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--fabric-jars", action="append", default=[],
                        metavar="DIR", help="directory of Fabric mod jars (repeatable)")
    parser.add_argument("--neoforge-jars", action="append", default=[],
                        metavar="DIR", help="directory of NeoForge mod jars (repeatable)")
    parser.add_argument("--server", type=Path, default=None,
                        help="an installed NeoForge server (its libraries enable --list-fml-mods)")
    parser.add_argument("--limit", type=int, default=DEFAULT_LIMIT,
                        help=f"maximum unique jars to probe (default {DEFAULT_LIMIT})")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT,
                        help=f"seconds per probe (default {DEFAULT_TIMEOUT:g})")
    parser.add_argument("--build-timeout", type=float, default=DEFAULT_BUILD_TIMEOUT,
                        help=f"seconds for the one upfront cargo build (default {DEFAULT_BUILD_TIMEOUT:g})")
    parser.add_argument("--bin-dir", type=Path, default=None,
                        help="use prebuilt horizon-native-java/horizon-jvm here instead of cargo run")
    parser.add_argument("--size-cap-mib", type=int, default=DEFAULT_SIZE_CAP_MIB,
                        help=f"skip jars larger than this many MiB (default {DEFAULT_SIZE_CAP_MIB})")
    parser.add_argument("--hash-budget-mib", type=int, default=DEFAULT_HASH_BUDGET_MIB,
                        help="stop hashing after this many MiB and defer the rest "
                             f"(default {DEFAULT_HASH_BUDGET_MIB})")
    parser.add_argument("--nested", action=argparse.BooleanOptionalAction, default=True,
                        help="probe nested Fabric JiJ / NeoForge JarJar jars (default: on)")
    parser.add_argument("--nested-limit", type=int, default=DEFAULT_NESTED_LIMIT,
                        help="maximum nested jars to probe per outer jar "
                             f"(default {DEFAULT_NESTED_LIMIT})")
    parser.add_argument("--nested-depth", type=int, default=DEFAULT_NESTED_DEPTH,
                        help=f"maximum nested-jar depth to recurse (default {DEFAULT_NESTED_DEPTH})")
    parser.add_argument("--nested-size-cap-mib", type=int, default=DEFAULT_NESTED_SIZE_CAP_MIB,
                        help="skip nested jars larger than this many MiB "
                             f"(default {DEFAULT_NESTED_SIZE_CAP_MIB})")
    parser.add_argument("--json", action="store_true", help="print the report as JSON only")
    parser.add_argument("--self-test", action="store_true",
                        help="probe a synthetic in-memory corpus (no JVM, no jars) and exit")
    args = parser.parse_args(argv)
    if args.limit < 1:
        parser.error("--limit must be at least 1")
    if args.timeout <= 0 or args.build_timeout <= 0:
        parser.error("--timeout and --build-timeout must be positive")
    if args.size_cap_mib < 1:
        parser.error("--size-cap-mib must be at least 1")
    if args.hash_budget_mib < 1:
        parser.error("--hash-budget-mib must be at least 1")
    if args.nested_limit < 1:
        parser.error("--nested-limit must be at least 1")
    if args.nested_depth < 0:
        parser.error("--nested-depth must not be negative")
    if args.nested_size_cap_mib < 1:
        parser.error("--nested-size-cap-mib must be at least 1")
    for path in [*args.fabric_jars, *args.neoforge_jars]:
        if not Path(path).is_dir():
            parser.error(f"{path} is not a directory")
    if args.bin_dir is not None and not args.bin_dir.is_dir():
        parser.error(f"{args.bin_dir} is not a directory")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    if args.self_test:
        return self_test()
    args.fabric_jars = [Path(path).resolve() for path in args.fabric_jars]
    args.neoforge_jars = [Path(path).resolve() for path in args.neoforge_jars]
    if args.server is not None:
        args.server = Path(args.server).resolve()
    if args.bin_dir is not None:
        args.bin_dir = Path(args.bin_dir).resolve()
    try:
        report = build_report(args)
    except HarnessError as error:
        print(f"real mod corpus: {error}", file=sys.stderr)
        return 1
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print_human(report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

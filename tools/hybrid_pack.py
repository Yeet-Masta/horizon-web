#!/usr/bin/env python3
"""Partition a Java mod pack for a hybrid native + Fabric-loader session.

The server ledger (`tools/java_mod_server_ledger.py`) labels every jar
`native-ok`, `needs-loader` or `native-gap`. This tool turns those labels into
an experiment:

* `native-ok` jars run through Horizon's native Java path;
* `needs-loader` jars run through the real Fabric loader, together with the
  dependency closure the loader half needs (a jar may therefore run in both
  halves — the two runtimes are separate processes);
* `native-gap` jars stay out unless a loader-half jar requires one.

It prints the partition, the two launch commands and a static check of each
jar's declared dependencies and content (no duplicate mod ids or registry
keys, no `breaks` between pack members, and every half's required ids present
in that half). `--measure` runs the native half alone (native-only) and then
both halves (hybrid) on a real pack, and reports how many mods actually ran.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
import re
import shlex
import signal
import subprocess
import sys
import time
import zipfile
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import lab  # noqa: E402

# The labels the server ledger writes. Older ledgers used `native` and
# `loader`; `read_ledger` normalises both.
NATIVE_OK = "native-ok"
NEEDS_LOADER = "needs-loader"
NATIVE_GAP = "native-gap"
LABELS = (NATIVE_OK, NEEDS_LOADER, NATIVE_GAP)
LEGACY_LABELS = {"native": NATIVE_OK, "loader": NEEDS_LOADER}

# Ids the loader/game itself provides, so no pack jar has to carry them.
PLATFORM_IDS = frozenset({
    "minecraft", "java", "fabricloader", "fabric-loader", "fabric", "neoforge",
    "forge", "javafml", "lowcodefml", "mixinextras",
})

# Corpus locations: everything lives under the developer's own corpus folder,
# never in the checkout and never in /tmp.
CORPUS = Path.home() / "horizon-corpus"
DEFAULT_GAME = CORPUS / "server-26.3-pinned.jar"
DEFAULT_LIBS = CORPUS / "fabric-libs-98"
DEFAULT_LOADER = DEFAULT_LIBS / "net" / "fabricmc" / "fabric-loader" / "0.20.1" / "fabric-loader-0.20.1.jar"
DEFAULT_NATIVE_BINARY = Path("target/fast/horizon-client")
DEFAULT_JVM_BINARY = Path("target/fast/horizon-jvm")
DEFAULT_RESOURCES = Path(".local/horizon-client/26.3")

MAX_REASON = 200
MAX_OUTPUT = 512 * 1024


def bounded(value: str, limit: int = MAX_REASON) -> str:
    text = " ".join(value.replace("\x00", "").split())
    return text if len(text) <= limit else text[: limit - 1] + "…"


# ---------------------------------------------------------------------------
# The ledger


@dataclass(frozen=True)
class LedgerRow:
    file: str
    label: str
    mod_id: str = "?"
    version: str = "?"
    verdict: str = ""
    skipped: int = 0
    reason: str = ""


def file_digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def build_hash_index(rows: dict[str, LedgerRow], directories: list[Path]) -> dict[str, LedgerRow]:
    """Map a jar's content digest to its ledger row.

    A pack folder keeps its own filenames, while the corpus ledger may have
    deduplicated the same bytes under another pack's name. Matching on content
    lets a pack use a ledger recorded elsewhere without renaming its jars.
    """
    index: dict[str, LedgerRow] = {}
    for name, row in rows.items():
        for directory in directories:
            candidate = directory / name
            if candidate.is_file():
                try:
                    index.setdefault(file_digest(candidate), row)
                except OSError:
                    pass
                break
    return index


def read_ledger(path: Path) -> dict[str, LedgerRow]:
    """Read the text ledger `java_mod_server_ledger.py --output` writes."""
    rows: dict[str, LedgerRow] = {}
    for number, line in enumerate(path.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
        if not line.strip() or line.startswith("summary\t"):
            continue
        fields = line.split("\t")
        if len(fields) < 2:
            continue
        values = dict(field.partition("=")[::2] for field in fields[1:])
        stored = values.get("recommend", "")
        label = LEGACY_LABELS.get(stored, stored)
        if label not in LABELS:
            raise ValueError(f"{path}:{number}: unknown label {stored!r}")
        try:
            skipped = int(values.get("skipped", "0") or 0)
        except ValueError as error:
            raise ValueError(f"{path}:{number}: skipped is not a number ({error})") from error
        rows[fields[0]] = LedgerRow(
            fields[0], label, values.get("id", "?"), values.get("version", "?"),
            values.get("verdict", ""), skipped, values.get("reason", ""),
        )
    if not rows:
        raise ValueError(f"{path}: no ledger rows")
    return rows


# ---------------------------------------------------------------------------
# What a jar declares


@dataclass(frozen=True)
class Declaration:
    path: Path
    mod_id: str
    version: str = "?"
    depends: tuple[str, ...] = ()
    breaks: tuple[str, ...] = ()
    content: tuple[tuple[str, str, str], ...] = ()
    nested_ids: tuple[str, ...] = ()
    client_only: bool = False
    error: str = ""


def _lenient_json(text: str) -> str:
    """Escape raw control characters inside JSON strings, as the native reader does."""
    out: list[str] = []
    in_string = escaped = False
    escapes = {"\n": "\\n", "\r": "\\r", "\t": "\\t", "\b": "\\b", "\f": "\\f"}
    for char in text:
        if not in_string:
            if char == '"':
                in_string = True
            out.append(char)
            continue
        if escaped:
            escaped = False
        elif char == "\\":
            escaped = True
        elif char == '"':
            in_string = False
        elif ord(char) < 0x20:
            out.append(escapes.get(char, f"\\u{ord(char):04x}"))
            continue
        out.append(char)
    return "".join(out)


def _id_list(value: object) -> tuple[str, ...]:
    """Dependency ids from either metadata shape: a dict's keys or a list."""
    if isinstance(value, dict):
        return tuple(str(key) for key in value)
    if isinstance(value, list):
        return tuple(str(item) for item in value if isinstance(item, str))
    return ()


def _native_content(raw: bytes) -> tuple[tuple[str, str, str], ...]:
    """Declared registry rows from `META-INF/horizon-native.json`."""
    value = json.loads(_lenient_json(raw.decode("utf-8", errors="replace")))
    entries = value.get("registrations", []) if isinstance(value, dict) else []
    content = []
    for entry in entries if isinstance(entries, list) else []:
        if not isinstance(entry, dict):
            continue
        registry = entry.get("registry")
        ident = entry.get("id")
        kind = entry.get("kind")
        if isinstance(registry, str) and isinstance(ident, str):
            content.append((registry, ident, kind if isinstance(kind, str) else ""))
    return tuple(sorted(set(content)))


def _toml_declarations(text: str) -> tuple[str, str, tuple[str, ...], bool]:
    """Bounded NeoForge/Forge metadata: first mod id, required deps, client-only."""
    ident = version = ""
    required: list[str] = []
    client_only = False
    section = "none"
    dependency_id = ""
    dependency_required = False

    def flush() -> None:
        nonlocal dependency_id, dependency_required
        if dependency_id and dependency_required:
            required.append(dependency_id)
        dependency_id = ""
        dependency_required = False

    for raw in text.splitlines():
        line = raw.strip()
        if line.startswith("[[mods]]"):
            flush()
            section = "mods" if not ident else "none"
            continue
        if line.startswith("[[dependencies."):
            flush()
            section = "dependency"
            continue
        if line.startswith("[[") or (line.startswith("[") and section != "none"):
            flush()
            section = "none"
            continue
        if "=" not in line:
            continue
        key, _, value = line.partition("=")
        key = key.strip()
        value = value.strip().strip('"').strip("'")
        if section == "mods":
            if key == "modId" and not ident:
                ident = value
            elif key == "version" and not version:
                version = value
            elif key == "clientSideOnly":
                client_only = value.lower() == "true"
        elif section == "dependency":
            if key == "modId":
                dependency_id = value
            elif key == "type":
                dependency_required = value.lower() == "required"
            elif key == "mandatory":
                dependency_required = value.lower() == "true"
    flush()
    return ident, version, tuple(sorted(set(required))), client_only


def _nested_ids(jar: zipfile.ZipFile) -> tuple[str, ...]:
    """Mod ids a jar carries as jar-in-jar modules (Fabric API's submodules)."""
    ids: list[str] = []
    for name in jar.namelist():
        if not (name.startswith("META-INF/jars/") and name.endswith(".jar")):
            continue
        try:
            with zipfile.ZipFile(io.BytesIO(jar.read(name))) as nested:
                raw = nested.read("fabric.mod.json")
                value = json.loads(_lenient_json(raw.decode("utf-8", errors="replace")))
                if isinstance(value, dict) and isinstance(value.get("id"), str):
                    ids.append(value["id"])
        except (KeyError, ValueError, OSError, zipfile.BadZipFile):
            continue
    return tuple(sorted(set(ids)))


def read_declaration(path: Path) -> Declaration:
    """One jar's mod id, required/breaking ids, declared content and nested ids."""
    try:
        with zipfile.ZipFile(path) as jar:
            names = set(jar.namelist())
            content: tuple[tuple[str, str, str], ...] = ()
            if "META-INF/horizon-native.json" in names:
                try:
                    content = _native_content(jar.read("META-INF/horizon-native.json"))
                except (ValueError, KeyError):
                    content = ()
            nested = _nested_ids(jar)
            if "fabric.mod.json" in names:
                raw = jar.read("fabric.mod.json").decode("utf-8", errors="replace")
                value = json.loads(_lenient_json(raw))
                if not isinstance(value, dict):
                    raise ValueError("fabric.mod.json root is not an object")
                ident = value.get("id")
                if not isinstance(ident, str):
                    raise ValueError("fabric.mod.json has no string id")
                depends = _id_list(value.get("depends", {})) + _id_list(value.get("requires", {}))
                breaks = _id_list(value.get("breaks", {})) + _id_list(value.get("conflicts", {}))
                return Declaration(
                    path, ident, str(value.get("version", "?")),
                    tuple(sorted(set(depends))), tuple(sorted(set(breaks))),
                    content, nested,
                    str(value.get("environment", "")).lower() == "client",
                )
            for name in ("META-INF/neoforge.mods.toml", "META-INF/mods.toml"):
                if name in names:
                    ident, version, depends, client_only = _toml_declarations(
                        jar.read(name).decode("utf-8", errors="replace"))
                    if not ident:
                        raise ValueError(f"{name} has no modId")
                    return Declaration(
                        path, ident, version or "?", depends, (), content, nested,
                        client_only,
                    )
    except (OSError, KeyError, ValueError, zipfile.BadZipFile) as error:
        # A jar the metadata reader cannot parse still belongs to the partition;
        # it just cannot contribute to the dependency/content check.
        return Declaration(path, path.stem, error=bounded(str(error)))
    return Declaration(path, path.stem, error="no fabric.mod.json or mods.toml")


# ---------------------------------------------------------------------------
# The partition


@dataclass
class PackJar:
    path: Path
    row: LedgerRow | None
    declaration: Declaration

    @property
    def name(self) -> str:
        return self.path.name

    @property
    def label(self) -> str:
        return self.row.label if self.row else "unlabelled"

    @property
    def mod_id(self) -> str:
        if self.declaration.mod_id and not self.declaration.error:
            return self.declaration.mod_id
        return self.row.mod_id if self.row and self.row.mod_id != "?" else self.path.stem


@dataclass
class Partition:
    pack: Path
    native: list[PackJar] = field(default_factory=list)
    loader: list[PackJar] = field(default_factory=list)
    excluded: list[PackJar] = field(default_factory=list)
    unlabelled: list[PackJar] = field(default_factory=list)

    @property
    def shared(self) -> list[PackJar]:
        native = {jar.path for jar in self.native}
        return [jar for jar in self.loader if jar.path in native]


def load_pack(pack: Path, rows: dict[str, LedgerRow], *, no_metadata: bool = False) -> list[PackJar]:
    jars = sorted((path for path in pack.iterdir() if path.suffix.lower() == ".jar"),
                  key=lambda path: path.name)
    out = []
    for path in jars:
        declaration = Declaration(path, path.stem, error="metadata not read") if no_metadata else read_declaration(path)
        out.append(PackJar(path, rows.get(path.name), declaration))
    return out


def _dependency_targets(jar: PackJar, pack_by_id: dict[str, list[PackJar]]) -> list[PackJar]:
    targets: list[PackJar] = []
    for dependency in jar.declaration.depends:
        key = dependency.lower()
        if key in PLATFORM_IDS:
            continue
        targets.extend(pack_by_id.get(key, []))
    return targets


def partition_pack(pack: Path, rows: dict[str, LedgerRow], *,
                   closure: bool = True, gap_to_loader: bool = False,
                   hash_index: dict[str, LedgerRow] | None = None) -> Partition:
    """Route each jar by its ledger label, then close the loader half's deps.

    A jar whose required dependency sits in the native half is added to the
    loader half too (the halves are separate processes, so the dependency must
    physically be there). With `gap_to_loader`, the jars native execution
    refuses are routed to the loader as well; otherwise only the closure can
    pull one in.
    """
    jars = load_pack(pack, rows)
    if hash_index:
        for jar in jars:
            if jar.row is None:
                try:
                    jar.row = hash_index.get(file_digest(jar.path))
                except OSError:
                    pass
    result = Partition(pack)
    pack_by_id: dict[str, list[PackJar]] = {}
    for jar in jars:
        pack_by_id.setdefault(jar.mod_id.lower(), []).append(jar)
        for nested in jar.declaration.nested_ids:
            pack_by_id.setdefault(nested.lower(), []).append(jar)

    for jar in jars:
        if jar.row is None:
            result.unlabelled.append(jar)
        elif jar.row.label == NATIVE_OK:
            result.native.append(jar)
        elif jar.row.label == NEEDS_LOADER or (gap_to_loader and jar.row.label == NATIVE_GAP):
            result.loader.append(jar)
        else:
            result.excluded.append(jar)

    if closure:
        chosen = {jar.path for jar in result.loader}
        queue = list(result.loader)
        while queue:
            jar = queue.pop(0)
            for target in _dependency_targets(jar, pack_by_id):
                if target.path in chosen:
                    continue
                chosen.add(target.path)
                queue.append(target)
                result.excluded = [item for item in result.excluded if item.path != target.path]
                result.unlabelled = [item for item in result.unlabelled if item.path != target.path]
        by_path = {jar.path: jar for jar in jars}
        result.loader = sorted((by_path[path] for path in chosen), key=lambda jar: jar.name)
    return result


# ---------------------------------------------------------------------------
# Declared content compatibility


@dataclass(frozen=True)
class Issue:
    severity: str  # "conflict" or "warning"
    code: str
    jars: tuple[str, ...]
    detail: str


def _half_provides(jars: list[PackJar]) -> set[str]:
    provided = set(PLATFORM_IDS)
    for jar in jars:
        provided.add(jar.mod_id.lower())
        provided.update(nested.lower() for nested in jar.declaration.nested_ids)
    return provided


def check_compatibility(part: Partition) -> list[Issue]:
    """Static checks over the pack's own declarations, per half and across it."""
    issues: list[Issue] = []
    jars = part.native + [jar for jar in part.loader if jar not in part.native] + part.excluded + part.unlabelled
    # Keep one entry per path even when a jar is routed to both halves.
    unique: dict[Path, PackJar] = {jar.path: jar for jar in jars}

    by_id: dict[str, list[str]] = {}
    content: dict[tuple[str, str], list[str]] = {}
    for jar in unique.values():
        if not jar.declaration.error:
            by_id.setdefault(jar.mod_id.lower(), []).append(jar.name)
        for registry, ident, _kind in jar.declaration.content:
            content.setdefault((registry.lower(), ident.lower()), []).append(jar.name)

    for ident, owners in sorted(by_id.items()):
        if len(owners) > 1:
            issues.append(Issue(
                "conflict", "duplicate-mod-id", tuple(sorted(owners)),
                f"{ident!r} is declared by {len(owners)} jars",
            ))
    for (registry, ident), owners in sorted(content.items()):
        if len(set(owners)) > 1:
            issues.append(Issue(
                "conflict", "duplicate-content", tuple(sorted(set(owners))),
                f"{registry}:{ident} is declared by {len(set(owners))} jars",
            ))

    # `breaks`/`conflicts` naming a member of the same pack. Fabric's field
    # carries a version range (`"<0.9.0"`), so without a semver evaluation this
    # is a warning: a check that ignored the range would flag every pack that
    # upgrades a dependency.
    for jar in unique.values():
        for broken in jar.declaration.breaks:
            owners = by_id.get(broken.lower(), [])
            if owners and owners != [jar.name]:
                issues.append(Issue(
                    "warning", "declared-break", tuple(sorted({jar.name, *owners})),
                    f"{jar.mod_id} declares it breaks {broken!r} (version range not evaluated)",
                ))

    # Each half must satisfy the dependencies of the jars it runs. The closure
    # makes this true by construction; what remains is a dependency no pack jar
    # supplies (or one only the excluded half supplies).
    pack_ids = set(by_id)
    for half, members in (("native", part.native), ("loader", part.loader)):
        provided = _half_provides(members)
        for jar in members:
            for dependency in jar.declaration.depends:
                key = dependency.lower()
                if key in provided:
                    continue
                if key in pack_ids:
                    issues.append(Issue(
                        "conflict", "cross-partition-dependency", (jar.name,),
                        f"{half} jar {jar.mod_id} needs {dependency!r}, which this half does not run",
                    ))
                else:
                    issues.append(Issue(
                        "warning", "missing-dependency", (jar.name,),
                        f"{half} jar {jar.mod_id} needs {dependency!r}, which no jar in the pack declares",
                    ))

    for jar in part.excluded + part.unlabelled:
        reason = "no ledger row" if jar.row is None else jar.row.reason
        severity = "warning"
        code = "unlabelled" if jar.row is None else "native-gap"
        issues.append(Issue(severity, code, (jar.name,), bounded(reason) or "left out of both halves"))

    # Deterministic order and no duplicate rows.
    seen: set[tuple[str, str, tuple[str, ...], str]] = set()
    out = []
    for issue in sorted(issues, key=lambda item: (item.severity != "conflict", item.code, item.jars, item.detail)):
        key = (issue.severity, issue.code, issue.jars, issue.detail)
        if key not in seen:
            seen.add(key)
            out.append(issue)
    return out


# ---------------------------------------------------------------------------
# The two launch commands


def native_command(jars: list[PackJar], *, binary: Path, resources: Path, work: Path,
                   port: int = 28000) -> list[str]:
    command = [str(binary), "--resources", str(resources),
               "--game-dir", str(work / "game"), "--home", str(work / "home")]
    # Everything after `--dedicated` is the server's own argv, so the mod
    # switches must come before it (as the server ledger's runner does).
    for jar in jars:
        command += ["--java-mod", str(jar.path)]
    command += ["--dedicated", "--nogui", "--port", str(port), "--world", "hybrid-world"]
    return command


def loader_command(jars: list[PackJar], *, binary: Path, loader: Path, libs: Path,
                   game: Path, work: Path) -> list[str]:
    command = [
        str(binary), "--real-loader", "--entry-kind", "server", "--launch-game",
        "--accept-eula", "--mixin-audit", "--loader", str(loader), "--libs", str(libs),
        "--game-jar", str(game), "--game-version", "26.3",
    ]
    command += [str(jar.path) for jar in jars]
    return command


def loader_env(loader: Path, libs: Path, game: Path, work: Path, base: dict[str, str]) -> dict[str, str]:
    env = dict(base)
    env.update({
        "HORIZON_REAL_LAUNCH": "1",
        "HORIZON_LAUNCH_STOP_AT_DONE": "1",
        "HORIZON_LAUNCH_DIR": str(work / "launch"),
        "HORIZON_MINECRAFT_JAR": str(game),
        "FABRIC_LOADER_JAR": str(loader),
        "FABRIC_LIBS_DIR": str(libs),
    })
    return env


def command_env_prefix(command: list[str], env: dict[str, str], names: tuple[str, ...]) -> str:
    pairs = [f"{name}={shlex.quote(env[name])}" for name in names if name in env]
    return "env " + " ".join(pairs) + " " if pairs else ""


LOADER_ENV_NAMES = (
    "HORIZON_REAL_LAUNCH", "HORIZON_LAUNCH_STOP_AT_DONE", "HORIZON_LAUNCH_DIR",
    "HORIZON_MINECRAFT_JAR", "FABRIC_LOADER_JAR", "FABRIC_LIBS_DIR",
)


def render_commands(part: Partition, *, native_binary: Path, jvm_binary: Path, resources: Path,
                    loader: Path, libs: Path, game: Path, work: Path,
                    port: int = 28000) -> dict[str, str]:
    native = native_command(part.native, binary=native_binary, resources=resources,
                            work=work / "native", port=port)
    boot = loader_command(part.loader, binary=jvm_binary, loader=loader, libs=libs,
                          game=game, work=work / "loader")
    # Only the six loader switches are rendered; the base environment is the
    # caller's job, so printing commands never creates a scratch folder.
    env = loader_env(loader, libs, game, work / "loader", {})
    prefix = command_env_prefix(boot, env, LOADER_ENV_NAMES)
    return {
        "native": shlex.join(native),
        "loader": prefix + shlex.join(boot),
    }


# ---------------------------------------------------------------------------
# Measurement


NATIVE_REPORT = re.compile(
    # The dedicated role writes `compatibility: <path>:`, the window
    # `compatibility <path>:`; both carry the same six fields.
    r"Java compatibility:? (.*?): engine=(\S+) loader=(\S+) "
    r"entrypoints=(\d+) registrations=(\d+) opaque=(\d+)"
)
LOADER_LISTING = re.compile(r"fabric-loader: Loading (\d+) mods \(([^)]*)\)")
LOADING_COUNT = re.compile(r"Loading (\d+) mods", re.I)
# A launched game log lists one mod per line as `\t- <id> <version>`; nested
# jar-in-jar modules use `|--` and are deliberately not top-level entries.
MOD_ENTRY = re.compile(r"^\s*-\s+([A-Za-z0-9_][\w.-]*)\s+\S+", re.M)
MIXIN_AUDIT = re.compile(r"^mixin-audit:\s+(.+)$", re.M)


@dataclass
class RunResult:
    half: str
    command: list[str]
    seconds: float
    exit_code: int | None
    timed_out: bool
    requested: int
    started: int = 0
    executing: int = 0
    loaded_ids: tuple[str, ...] = ()
    woven: int = 0
    skipped: int = 0
    report: dict[str, dict[str, object]] = field(default_factory=dict)
    log: Path | None = None
    note: str = ""


def _kill(proc: subprocess.Popen[bytes]) -> None:
    if proc.poll() is not None:
        return
    try:
        os.killpg(proc.pid, signal.SIGTERM)
        proc.wait(timeout=1.5)
    except (ProcessLookupError, subprocess.TimeoutExpired):
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def _run(command: list[str], *, work: Path, env: dict[str, str], timeout: float,
         stop_at_done: bool) -> tuple[str, float, int | None, bool, Path]:
    work.mkdir(parents=True, exist_ok=True)
    log_path = work / "run.log"
    start = time.monotonic()
    with log_path.open("wb") as log:
        proc = subprocess.Popen(command, cwd=work, env=env, stdout=log,
                                stderr=subprocess.STDOUT, start_new_session=True)
        timed_out = False
        deadline = start + timeout
        while proc.poll() is None:
            if time.monotonic() >= deadline:
                timed_out = True
                _kill(proc)
                break
            if stop_at_done:
                try:
                    if b"Done (" in log_path.read_bytes()[-MAX_OUTPUT:]:
                        _kill(proc)
                        break
                except OSError:
                    pass
            time.sleep(0.05)
    elapsed = time.monotonic() - start
    try:
        with log_path.open("rb") as log:
            log.seek(0, os.SEEK_END)
            log.seek(max(0, log.tell() - MAX_OUTPUT))
            text = log.read(MAX_OUTPUT).decode("utf-8", errors="replace")
    except OSError:
        text = ""
    return text, elapsed, proc.returncode, timed_out, log_path


def parse_native(text: str, home: Path, requested: int) -> dict[str, object]:
    """Native reports from the log and `HORIZON_HOME/java-runtime.json`."""
    reports: dict[str, dict[str, object]] = {}
    for match in NATIVE_REPORT.finditer(text):
        jar, engine, loader, entrypoints, registrations, opaque = match.groups()
        name = Path(jar).name
        reports[name] = {
            "engine": engine, "loader": loader, "entrypoints": int(entrypoints),
            "registrations": int(registrations), "opaque": int(opaque),
            "woven": 0, "skipped": 0,
        }
    runtime = home / "java-runtime.json"
    if runtime.is_file():
        try:
            value = json.loads(runtime.read_text(encoding="utf-8"))
            for row in value.get("jars", []):
                name = str(row.get("file", ""))
                if name in reports:
                    reports[name]["woven"] = int(row.get("woven", 0))
                    reports[name]["skipped"] = int(row.get("skipped", 0))
                    reports[name]["engine"] = str(row.get("engine", reports[name]["engine"]))
        except (OSError, ValueError, TypeError):
            pass
    started = sum(1 for row in reports.values() if row["engine"] != "refused")
    executing = sum(1 for row in reports.values()
                    if row["engine"] != "refused" and int(row["entrypoints"]) > 0)
    return {
        "report": reports,
        "started": started,
        "executing": executing,
        "woven": sum(int(row["woven"]) for row in reports.values()),
        "skipped": sum(int(row["skipped"]) for row in reports.values()),
        "note": "" if reports else "no Java compatibility lines",
    }


def parse_loader(text: str, jars: list[PackJar], requested: int) -> dict[str, object]:
    listing: tuple[str, ...] = ()
    total = 0
    for match in LOADER_LISTING.finditer(text):
        total = int(match.group(1))
        listing = tuple(item for item in match.group(2).split(",") if item)
    if not listing:
        # `--launch-game` prints the listing into the game's own log instead of
        # the `fabric-loader:` probe line.
        listing = tuple(MOD_ENTRY.findall(text))
        counts = LOADING_COUNT.findall(text)
        if counts:
            total = int(counts[-1])
    # Count the jars the loader half was created for, not the native-ok
    # dependencies closure added to make them boot.
    wanted = {jar.mod_id.lower() for jar in jars
              if jar.label != NATIVE_OK and not jar.declaration.error}
    loaded = tuple(sorted(ident for ident in listing if ident.lower() in wanted))
    audit = ""
    applied = 0
    for match in MIXIN_AUDIT.finditer(text):
        audit = match.group(1).strip()
        applied = int(re.search(r"applied=(\d+)", audit).group(1)) if "applied=" in audit else 0
    done = "Done (" in text
    return {
        # A half that never reached `Done` executed no mod to the point the
        # server could use it, whatever its listing said.
        "started": len(loaded) if done else 0,
        "executing": len(loaded) if done else 0,
        "loaded_ids": loaded,
        "woven": applied,
        "note": ("Done (" if done else "no Done (") + (f"; loading={total}" if total else "")
                + (f"; mixin-audit={audit}" if audit else ""),
    }


def run_native(part: Partition, *, binary: Path, resources: Path, work: Path,
               timeout: float, port: int) -> RunResult:
    if not part.native:
        return RunResult("native", [], 0.0, None, False, 0, note="empty half")
    command = native_command(part.native, binary=binary, resources=resources, work=work, port=port)
    for directory in (work / "game", work / "home"):
        directory.mkdir(parents=True, exist_ok=True)
    (work / "game" / "eula.txt").write_text("eula=true\n", encoding="utf-8")
    (work / "game" / "server.properties").write_text(
        f"level-type=minecraft:flat\nonline-mode=false\nserver-port={port}\n", encoding="utf-8")
    env = lab.game_env(
        HORIZON_HOME=work / "home",
        HORIZON_LAUNCHER_HOME=work / "launcher-home",
        XDG_DATA_HOME=work / "xdg-data",
        XDG_CONFIG_HOME=work / "xdg-config",
    )
    env["TMPDIR"] = str(work / "tmp")
    (work / "tmp").mkdir(parents=True, exist_ok=True)
    text, seconds, code, timed_out, log = _run(
        command, work=work, env=env, timeout=timeout, stop_at_done=True)
    parsed = parse_native(text, work / "home", len(part.native))
    return RunResult("native", command, seconds, code, timed_out, len(part.native),
                     log=log, **parsed)


def _append_child_log(text: str, work: Path) -> str:
    """Fabric's mod listing, `Done (` and Mixin's lines go to the game log."""
    log = work / "launch" / "game" / "logs" / "latest.log"
    try:
        with log.open("rb") as stream:
            size = log.stat().st_size
            if size <= 4 * MAX_OUTPUT:
                child = stream.read().decode("utf-8", errors="replace")
            else:
                # Keep the head (the `Loading N mods` listing) and the tail
                # (`Done (` and the final failures) of a verbose Mixin log.
                head = stream.read(MAX_OUTPUT)
                stream.seek(max(0, size - MAX_OUTPUT))
                child = (head + b"\n" + stream.read()).decode("utf-8", errors="replace")
    except OSError:
        return text
    return text + "\n" + child


def run_loader(part: Partition, *, binary: Path, loader: Path, libs: Path, game: Path,
               work: Path, timeout: float) -> RunResult:
    if not part.loader:
        return RunResult("loader", [], 0.0, None, False, 0, note="empty half")
    command = loader_command(part.loader, binary=binary, loader=loader, libs=libs,
                             game=game, work=work)
    work.mkdir(parents=True, exist_ok=True)
    env = loader_env(loader, libs, game, work, lab.game_env(
        HORIZON_HOME=work / "home",
        HORIZON_LAUNCHER_HOME=work / "launcher-home",
        XDG_DATA_HOME=work / "xdg-data",
        XDG_CONFIG_HOME=work / "xdg-config",
    ))
    env["TMPDIR"] = str(work / "tmp")
    (work / "tmp").mkdir(parents=True, exist_ok=True)
    text, seconds, code, timed_out, log = _run(
        command, work=work, env=env, timeout=timeout, stop_at_done=True)
    text = _append_child_log(text, work)
    parsed = parse_loader(text, part.loader, len(part.loader))
    return RunResult("loader", command, seconds, code, timed_out, len(part.loader),
                     log=log, **parsed)


# ---------------------------------------------------------------------------
# Rendering


def partition_lines(part: Partition) -> list[str]:
    rows = [f"pack: {part.pack}"]
    for name, members in (("native", part.native), ("loader", part.loader),
                          ("excluded", part.excluded), ("unlabelled", part.unlabelled)):
        rows.append(f"{name}: {len(members)}")
        for jar in members:
            tags = [jar.label]
            if name == "loader" and jar in part.native:
                tags.append("shared")
            reason = bounded(jar.row.reason) if jar.row and jar.row.reason else ""
            rows.append(f"  {jar.name}\t{jar.mod_id}\t{'/'.join(tags)}" + (f"\t{reason}" if reason else ""))
    return rows


def compatibility_lines(issues: list[Issue]) -> list[str]:
    conflicts = [issue for issue in issues if issue.severity == "conflict"]
    warnings = [issue for issue in issues if issue.severity == "warning"]
    lines = [f"declared content: {len(conflicts)} conflict(s), {len(warnings)} warning(s)"]
    for issue in issues:
        lines.append(f"  {issue.severity}\t{issue.code}\t{', '.join(issue.jars)}\t{issue.detail}")
    return lines


def measure(part: Partition, *, native_binary: Path, jvm_binary: Path, resources: Path,
            loader: Path, libs: Path, game: Path, work: Path, timeout: float,
            port: int) -> dict[str, object]:
    """Run native-only, then the same native half beside the loader half."""
    # `HORIZON_LAUNCH_DIR` must be fresh, and a reused scratch tree must not
    # hand a run an earlier run's game log, so every measurement gets its own
    # folder under the caller's work root.
    root = work / f"{time.strftime('%Y%m%d-%H%M%S')}-{os.getpid()}"
    native_only = run_native(part, binary=native_binary, resources=resources,
                             work=root / "native-only", timeout=timeout, port=port)
    hybrid_native = run_native(part, binary=native_binary, resources=resources,
                               work=root / "hybrid-native", timeout=timeout, port=port + 1)
    hybrid_loader = run_loader(part, binary=jvm_binary, loader=loader, libs=libs, game=game,
                               work=root / "hybrid-loader", timeout=timeout)
    native_ids = {name for name, row in native_only.report.items()
                  if row["engine"] != "refused" and int(row["entrypoints"]) > 0}
    hybrid_ids = set(native_ids)
    if hybrid_loader.started:
        hybrid_ids |= set(hybrid_loader.loaded_ids)
    return {
        "native_only": native_only,
        "hybrid_native": hybrid_native,
        "hybrid_loader": hybrid_loader,
        "native_mods": len(native_ids),
        "hybrid_mods": len(hybrid_ids),
        "native_woven": native_only.woven,
        "hybrid_woven": hybrid_native.woven + hybrid_loader.woven,
        "hybrid_mod_ids": sorted(hybrid_ids),
        "work": root,
    }


def measurement_lines(result: dict[str, object]) -> list[str]:
    rows = ["measurement (server role, one process per half):",
            f"{'run':<14}{'jars':>5}{'started':>8}{'executing':>10}{'woven':>7}{'skipped':>8}{'secs':>8}  note"]
    for key in ("native_only", "hybrid_native", "hybrid_loader"):
        run: RunResult = result[key]
        rows.append(f"{key:<14}{run.requested:>5}{run.started:>8}{run.executing:>10}"
                    f"{run.woven:>7}{run.skipped:>8}{run.seconds:>8.2f}  {run.note}")
    rows.append(f"native-only effective mods: {result['native_mods']}")
    rows.append(f"hybrid effective mods:      {result['hybrid_mods']}")
    return rows


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("pack", type=Path, help="directory containing the pack's jars")
    parser.add_argument("--ledger", type=Path, required=True, action="append",
                        help="text ledger from java_mod_server_ledger.py (repeatable)")
    parser.add_argument("--match-by-hash", action="store_true",
                        help="match an unlabelled jar to a ledger row by its content digest")
    parser.add_argument("--ledger-dir", type=Path, action="append",
                        help="where the ledger's jars live for --match-by-hash "
                             "(default: corpus-xl, mega, mega-packs under ~/horizon-corpus)")
    parser.add_argument("--no-closure", action="store_true",
                        help="do not add the loader half's dependency closure")
    parser.add_argument("--gap-to-loader", action="store_true",
                        help="route `native-gap` jars to the loader too (full hybrid)")
    parser.add_argument("--commands", action="store_true", help="print the two launch commands")
    parser.add_argument("--measure", action="store_true", help="run native-only and hybrid")
    parser.add_argument("--json", action="store_true", dest="json_output")
    parser.add_argument("--strict", action="store_true",
                        help="exit 1 when the declared-content check finds a conflict")
    parser.add_argument("--work-dir", type=Path,
                        help="scratch root (default: ~/horizon-corpus/hybrid-runs/<pack>-<time>)")
    parser.add_argument("--native-binary", type=Path, default=DEFAULT_NATIVE_BINARY)
    parser.add_argument("--jvm-binary", type=Path, default=DEFAULT_JVM_BINARY)
    parser.add_argument("--resources", type=Path, default=DEFAULT_RESOURCES)
    parser.add_argument("--game", type=Path, default=DEFAULT_GAME)
    parser.add_argument("--libs", type=Path, default=DEFAULT_LIBS)
    parser.add_argument("--loader", type=Path, default=DEFAULT_LOADER)
    parser.add_argument("--timeout", type=float, default=180.0)
    parser.add_argument("--port", type=int, default=28600)
    args = parser.parse_args(argv)

    if not args.pack.is_dir():
        parser.error(f"pack directory does not exist: {args.pack}")
    # The measured processes run with their scratch folder as the working
    # directory, so every path they receive must be absolute.
    for name in ("native_binary", "jvm_binary", "resources", "game", "libs", "loader", "pack"):
        setattr(args, name, getattr(args, name).resolve())
    args.ledger = [path.resolve() for path in args.ledger]
    if args.ledger_dir:
        args.ledger_dir = [path.resolve() for path in args.ledger_dir]
    rows: dict[str, LedgerRow] = {}
    for ledger in args.ledger:
        if not ledger.is_file():
            parser.error(f"ledger does not exist: {ledger}")
        try:
            rows.update(read_ledger(ledger))
        except ValueError as error:
            parser.error(str(error))
    hash_index = None
    if args.match_by_hash:
        directories = args.ledger_dir or [
            CORPUS / "corpus-xl", CORPUS / "mega", CORPUS / "mega-packs"]
        hash_index = build_hash_index(rows, directories)
    part = partition_pack(args.pack, rows, closure=not args.no_closure,
                          gap_to_loader=args.gap_to_loader, hash_index=hash_index)
    if not part.native and not part.loader:
        parser.error("the ledger routes no jar in this pack to either half")
    issues = check_compatibility(part)
    conflicts = sum(issue.severity == "conflict" for issue in issues)

    work = args.work_dir
    if work is None:
        stamp = time.strftime("%Y%m%d-%H%M%S")
        work = CORPUS / "hybrid-runs" / f"{args.pack.name}-{stamp}"
    commands = render_commands(
        part, native_binary=args.native_binary, jvm_binary=args.jvm_binary,
        resources=args.resources, loader=args.loader, libs=args.libs, game=args.game,
        work=work, port=args.port,
    )

    result = None
    if args.measure:
        missing = [name for name, path in (
            ("native binary", args.native_binary), ("jvm binary", args.jvm_binary),
            ("resources", args.resources), ("game jar", args.game),
            ("loader", args.loader), ("libraries", args.libs)) if not path.exists()]
        if missing:
            parser.error("cannot measure, missing: " + ", ".join(missing))
        result = measure(
            part, native_binary=args.native_binary, jvm_binary=args.jvm_binary,
            resources=args.resources, loader=args.loader, libs=args.libs, game=args.game,
            work=work, timeout=args.timeout, port=args.port,
        )

    if args.json_output:
        payload: dict[str, object] = {
            "pack": str(part.pack),
            "ledger": [str(path) for path in args.ledger],
            "native": [jar.name for jar in part.native],
            "loader": [jar.name for jar in part.loader],
            "excluded": [jar.name for jar in part.excluded],
            "unlabelled": [jar.name for jar in part.unlabelled],
            "shared": [jar.name for jar in part.shared],
            "issues": [issue.__dict__ for issue in issues],
            "commands": commands,
            "work_dir": str(work),
        }
        if result is not None:
            payload["measurement"] = {
                "native_mods": result["native_mods"],
                "hybrid_mods": result["hybrid_mods"],
                "hybrid_mod_ids": result["hybrid_mod_ids"],
                "runs": {
                    key: {
                        "requested": getattr(result[key], "requested"),
                        "started": getattr(result[key], "started"),
                        "executing": getattr(result[key], "executing"),
                        "woven": getattr(result[key], "woven"),
                        "skipped": getattr(result[key], "skipped"),
                        "seconds": round(getattr(result[key], "seconds"), 3),
                        "timed_out": getattr(result[key], "timed_out"),
                        "note": getattr(result[key], "note"),
                        "log": str(getattr(result[key], "log") or ""),
                    }
                    for key in ("native_only", "hybrid_native", "hybrid_loader")
                },
            }
        print(json.dumps(payload, indent=2, sort_keys=True))
    else:
        print("\n".join(partition_lines(part)))
        print()
        print("\n".join(compatibility_lines(issues)))
        if args.commands:
            print()
            print("# native-only (the native half):")
            print(commands["native"])
            print()
            print("# hybrid: the same native half, plus the loader half")
            print(commands["loader"])
        if result is not None:
            print()
            print("\n".join(measurement_lines(result)))
            print(f"logs under: {result['work']}")
    return 1 if args.strict and conflicts else 0


if __name__ == "__main__":
    raise SystemExit(main())

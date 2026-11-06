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

`--coverage` turns the same ledgers into a tier-coverage ledger over whole
corpora (`corpus-xl`, `mega`): every jar is `native-ok`, `needs-loader` or
`native-gap`, and a `needs-loader` row only counts when a real Fabric loader
launch lists the jar and reaches `Done`. `--measure-proof` drives those loader
launches for the rows that have no proof yet, batching a jar with its declared
dependency closure and splitting a batch that cannot boot. `--docs` writes the
generated table into `docs/java-mod-compat-status.md`; `--self-test` checks the
classification against synthetic jars and ledgers without a game.

`--native-score` is the phase-2 scoreboard. It takes every `needs-loader` row
(the jars the loader tier carries, proven by the loader proof ledgers), runs
each unique jar again with the native-only dedicated server, and asks how many
of them no longer need the loader: the run must reach `Done (`, execute a
server entrypoint, and have no server-side plan whose target is outside the
jar. It writes `tools/real_mod_fixtures/LAST_NATIVE_SCORE.txt`, appends one
measurement to `tools/real_mod_fixtures/NATIVE_SCORE_HISTORY.txt`, and rewrites
the trend block in `docs/java-mod-compat-status.md`. A duplicate content digest
across corpora is run once and shares its outcome.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
import re
import shlex
import shutil
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
    failure: str = ""


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


HARD_DEP = re.compile(r"Immediate reason: \[HARD_DEP\w* (\S+)(?: \S+)? \{depends ([\w-]+) @ \[([^\]]+)\]")
CLIENT_CLASS = re.compile(r"net/minecraft/client/|net\.minecraft\.client\.")


def failure_hint(text: str) -> str:
    """A bounded cause from a loader run that did not reach `Done`."""
    if CLIENT_CLASS.search(text):
        return "client-only code on a server"
    match = HARD_DEP.search(text)
    if match:
        _, dependency, requirement = match.groups()
        if dependency == "minecraft":
            return f"requires minecraft {requirement}"
        return f"missing dependency {dependency} {requirement}"
    if "HARD_DEP_NO_CANDIDATE" in text:
        match = re.search(r"HARD_DEP_NO_CANDIDATE (\S+)", text)
        return f"unresolved dependency {match.group(1)}" if match else "unresolved dependency"
    if "Mod resolution failed" in text:
        return "mod resolution failed"
    return ""


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
    # A reused work folder must not hand this run an earlier run's game log:
    # `_append_child_log` would append its `Loading N mods` and `Done (` lines
    # and make a failed launch look successful.
    if (work / "launch").exists():
        shutil.rmtree(work / "launch", ignore_errors=True)
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
                     log=log, failure=failure_hint(text), **parsed)


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


# ---------------------------------------------------------------------------
# Corpus tier coverage


COVERAGE_DOC_START = "<!-- java-mod-coverage:start -->"
COVERAGE_DOC_END = "<!-- java-mod-coverage:end -->"

# Proof states a `needs-loader` row can carry.
PROVEN = "proven"          # listed by a loader run that reached `Done (`
PARTIAL = "partial"        # listed, but that run never reached `Done (`
UNPROVEN = "unproven"      # no loader run listed the jar
NOT_NEEDED = "not-needed"  # a native-ok row

# A jar built for another game version cannot run on this one in either tier;
# it is a corpus-hygiene finding, not a compatibility gap.
OUT_OF_SCOPE = "out-of-scope"


@dataclass(frozen=True)
class LoaderRow:
    """One jar's result in a real Fabric loader run."""

    jar: str
    mod_id: str
    status: str
    mixins: int = 0
    done: bool = False
    source: str = ""
    error: str = ""

    @property
    def listed(self) -> bool:
        return self.status == "listed"

    @property
    def rank(self) -> int:
        if self.listed and self.done:
            return 3
        if self.listed:
            return 2
        if self.status == "error":
            return 1
        return 0


def read_loader_ledger(path: Path) -> list[LoaderRow]:
    """Read `java_mod_loader_ledger.py`'s text ledger (or a proof ledger).

    The tool writes one `done=` for the whole run; the coverage proof ledger
    adds a per-row `done=` because a sweep runs one loader process per batch.
    """
    text = path.read_text(encoding="utf-8", errors="replace")
    done = False
    for line in text.splitlines():
        if line.startswith("summary\t"):
            values = dict(field.partition("=")[::2] for field in line.split("\t")[1:])
            done = values.get("done", "false").lower() == "true"
            break
    rows: list[LoaderRow] = []
    for number, line in enumerate(text.splitlines(), 1):
        if not line.startswith("mod="):
            continue
        fields = line.split("\t")
        mod_id = fields[0].partition("=")[2]
        values = dict(field.partition("=")[::2] for field in fields[1:])
        try:
            mixins = int(values.get("mixins", "0") or 0)
        except ValueError as error:
            raise ValueError(f"{path}:{number}: mixins is not a number ({error})") from error
        row_done = values.get("done")
        rows.append(LoaderRow(
            values.get("jar", ""), mod_id, values.get("status", ""), mixins,
            done if row_done is None else row_done.lower() == "true", str(path),
            values.get("error", ""),
        ))
    if not rows:
        raise ValueError(f"{path}: no loader rows")
    return rows


def merge_loader_evidence(rows: list[LoaderRow]) -> dict[str, LoaderRow]:
    """Best row per jar name.

    A corpus sweep passes the same file to the loader it measured, so the jar
    name is the identity. A pack with renamed jars needs a content digest to
    match, which the coverage report does not guess from a mod id: two versions
    of one mod share an id and only one of them was launched.
    """
    by_jar: dict[str, LoaderRow] = {}
    for row in rows:
        if not row.jar:
            continue
        current = by_jar.get(row.jar)
        if (current is None or row.rank > current.rank
                or (row.rank == current.rank and row.error and not current.error)):
            by_jar[row.jar] = row
    return by_jar


@dataclass(frozen=True)
class CoverageRow:
    corpus: str
    jar: str
    mod_id: str
    digest: str
    native_label: str
    tier: str
    proof: str
    loader_mod_id: str
    mixins: int
    cause: str
    reason: str


def _loader_evidence(jar: str, by_jar: dict[str, LoaderRow]) -> LoaderRow | None:
    return by_jar.get(jar)


def native_cause(reason: str) -> str:
    """The server ledger's refusal categories, in one word."""
    text = reason.strip()
    if not text:
        return "no server ledger row"
    if text.startswith("Metadata("):
        return "metadata"
    if text.startswith("uncaught exception"):
        return "runtime"
    if "StackUnderflow" in text or "verifier" in text.lower():
        return "verifier"
    if "nested" in text.lower() and "bound" in text.lower():
        return "interpreter bound"
    if text.startswith("duplicate"):
        return "declaration"
    if text.startswith("no native entrypoint"):
        return "no entrypoint"
    return "other"


def classify_jar(corpus: str, jar: PackJar, digest: str,
                 by_jar: dict[str, LoaderRow]) -> CoverageRow:
    """One jar's tier: native, loader (only when proven), or a cause."""
    row = jar.row
    label = row.label if row else "unlabelled"
    mod_id = jar.mod_id
    proof_row = _loader_evidence(jar.name, by_jar)
    if label == NATIVE_OK:
        return CoverageRow(corpus, jar.name, mod_id, digest, label, NATIVE_OK,
                           NOT_NEEDED, proof_row.mod_id if proof_row else "", 0, "",
                           bounded(row.reason) if row else "")
    if proof_row is not None and proof_row.error.startswith("requires minecraft"):
        return CoverageRow(corpus, jar.name, mod_id, digest, label, OUT_OF_SCOPE,
                           NOT_NEEDED, proof_row.mod_id, 0, "minecraft-version",
                           f"built for another game version ({proof_row.error}); it cannot run on 26.3")
    if label == NATIVE_GAP and row is not None and row.reason.startswith("no native entrypoint"):
        # The server role has nothing to execute; the ledger's own `native-ok`
        # rows with that reason are counted the same way.
        return CoverageRow(corpus, jar.name, mod_id, digest, label, NATIVE_OK,
                           NOT_NEEDED, proof_row.mod_id if proof_row else "", 0, "no entrypoint",
                           "no native entrypoint; the dedicated server has nothing to run")
    if jar.declaration.client_only:
        # Fabric never loads an `environment: client` mod on a dedicated
        # server, whatever entrypoint kinds it declares. The server role has
        # nothing to run, exactly like the client-only rows the ledger already
        # counts as native-ok.
        return CoverageRow(corpus, jar.name, mod_id, digest, label, NATIVE_OK,
                           NOT_NEEDED, proof_row.mod_id if proof_row else "", 0, "client-only",
                           "client-only (`environment: client`); the dedicated server loads nothing of it")
    if proof_row is not None and proof_row.listed:
        proof = PROVEN if proof_row.done else PARTIAL
        if proof_row.done:
            note = "the real Fabric loader lists it and reached Done"
        else:
            note = "the real Fabric loader lists it but the run did not boot"
            if proof_row.error:
                note += f" ({proof_row.error})"
        native = "" if label == NEEDS_LOADER else f"native refused ({native_cause(row.reason if row else '')})"
        reason = bounded("; ".join(part for part in (native, note) if part))
        return CoverageRow(corpus, jar.name, mod_id, digest, label, NEEDS_LOADER, proof,
                           proof_row.mod_id, proof_row.mixins, native_cause(row.reason if row else ""),
                           reason)
    # Neither native nor the loader carries it: keep the native cause and say
    # what the loader did (or that no run covered it).
    local = "" if label == NEEDS_LOADER else (row.reason if row else "")
    loader = ("the loader did not list it" if proof_row is not None
              else "no loader run covered it")
    if label == NEEDS_LOADER:
        native = "the server runs it"
    elif label == "unlabelled":
        native = "no server ledger row"
    else:
        native = f"native refused ({native_cause(local)}): {bounded(local, 160)}" if local \
            else f"native refused ({native_cause('')})"
    cause = "" if label == NEEDS_LOADER else native_cause(local)
    if proof_row is not None and proof_row.error:
        loader = proof_row.error
        if proof_row.error.startswith("requires minecraft"):
            cause = "minecraft-version"
        elif proof_row.error.startswith("client-only"):
            cause = "client-only code"
        elif "dependency" in proof_row.error:
            cause = "missing dependency"
    return CoverageRow(corpus, jar.name, mod_id, digest, label, NATIVE_GAP,
                       UNPROVEN, proof_row.mod_id if proof_row else "", 0, cause,
                       bounded(f"{native}; {loader}"))


def coverage_rows(corpora: list[tuple[str, Path]], rows: dict[str, LedgerRow],
                  evidence: list[LoaderRow], *, digests: dict[Path, str] | None = None
                  ) -> list[CoverageRow]:
    by_jar = merge_loader_evidence(evidence)
    out: list[CoverageRow] = []
    for name, directory in corpora:
        for jar in load_pack(directory, rows):
            digest = ""
            if digests is not None:
                digest = digests.get(jar.path) or file_digest(jar.path)
                digests[jar.path] = digest
            out.append(classify_jar(name, jar, digest, by_jar))
    return out


def union_rows(rows: list[CoverageRow]) -> list[CoverageRow]:
    """One row per jar content, keeping the best tier a corpus measured."""
    best: dict[str, CoverageRow] = {}
    order = {NATIVE_OK: 3, NEEDS_LOADER: 2, OUT_OF_SCOPE: 1, NATIVE_GAP: 0}

    def score(row: CoverageRow) -> tuple[int, int]:
        proof = {PROVEN: 2, NOT_NEEDED: 2, PARTIAL: 1, UNPROVEN: 0}[row.proof]
        return order[row.tier], proof

    for row in rows:
        key = row.digest or f"{row.corpus}/{row.jar}"
        current = best.get(key)
        if current is None or score(row) > score(current):
            best[key] = row
    return sorted(best.values(), key=lambda row: (row.tier, row.jar))


def coverage_counts(rows: list[CoverageRow]) -> dict[str, int]:
    counts = {"jars": len(rows), NATIVE_OK: 0, NEEDS_LOADER: 0, NATIVE_GAP: 0,
              OUT_OF_SCOPE: 0, PROVEN: 0, PARTIAL: 0, UNPROVEN: 0}
    for row in rows:
        counts[row.tier] += 1
        if row.tier == NEEDS_LOADER:
            counts[row.proof] += 1
    return counts


def coverage_conflicts(rows: list[CoverageRow]) -> list[tuple[str, list[CoverageRow]]]:
    by_digest: dict[str, list[CoverageRow]] = {}
    for row in rows:
        if row.digest:
            by_digest.setdefault(row.digest, []).append(row)
    return [(digest, group) for digest, group in by_digest.items()
            if len({row.tier for row in group}) > 1]


def coverage_payload(rows: list[CoverageRow], union: list[CoverageRow]) -> dict[str, object]:
    corpora: dict[str, dict[str, int]] = {}
    for row in rows:
        corpora.setdefault(row.corpus, {"jars": 0, NATIVE_OK: 0, NEEDS_LOADER: 0,
                                        NATIVE_GAP: 0, OUT_OF_SCOPE: 0})
        corpora[row.corpus]["jars"] += 1
        corpora[row.corpus][row.tier] += 1
    return {
        "corpora": corpora,
        "union": coverage_counts(union),
        "conflicts": len(coverage_conflicts(rows)),
        "rows": [row.__dict__ for row in rows],
    }


# ---------------------------------------------------------------------------
# Driving the loader proof runs


VERSION_PART = re.compile(r"\d+")


def _version_key(version: str) -> tuple[int, ...]:
    """A sortable version: the numeric parts, so a newer provider wins.

    A corpus holds several versions of one mod (e.g. three `fabric-api`s). The
    loader refuses a pack that carries two, and a dependent usually wants the
    newest, so closure picks the highest numeric version and only falls back to
    the name for ties.
    """
    if version in ("", "?"):
        return (0,)
    parts = tuple(int(part) for part in VERSION_PART.findall(version)[:6])
    return parts or (0,)


def _provider_index(jars: list[PackJar]) -> dict[str, list[PackJar]]:
    """Mod ids (and nested module ids) each jar can satisfy, newest first."""
    index: dict[str, list[PackJar]] = {}
    for jar in jars:
        ids = {jar.mod_id.lower()} | {nested.lower() for nested in jar.declaration.nested_ids}
        for ident in ids:
            index.setdefault(ident, []).append(jar)
    for ident, providers in index.items():
        # A jar whose own mod id *is* the dependency is the canonical provider;
        # then a jar that carries the id as a nested module (fabric-api carries
        # 44 submodules, but so do mods bundling a single one); only then does
        # the newest version win.
        providers.sort(key=lambda jar: (jar.mod_id.lower() == ident,
                                        len(jar.declaration.nested_ids),
                                        _version_key(jar.declaration.version), jar.name),
                       reverse=True)
    return index


def _closure(batch: list[PackJar], index: dict[str, list[PackJar]]) -> list[PackJar]:
    """The batch plus every declared dependency it can resolve in the corpus."""
    chosen = {jar.path: jar for jar in batch}
    ids = {jar.mod_id.lower() for jar in batch}
    for jar in batch:
        ids |= {nested.lower() for nested in jar.declaration.nested_ids}
    queue = list(batch)
    while queue:
        jar = queue.pop(0)
        for dependency in jar.declaration.depends:
            key = dependency.lower()
            if key in PLATFORM_IDS or key in ids:
                continue
            providers = [provider for provider in index.get(key, [])
                         if provider.path not in chosen
                         and provider.mod_id.lower() not in ids]
            if not providers:
                continue
            provider = providers[0]
            chosen[provider.path] = provider
            ids.add(provider.mod_id.lower())
            ids |= {nested.lower() for nested in provider.declaration.nested_ids}
            queue.append(provider)
    return sorted(chosen.values(), key=lambda jar: jar.name)


def plan_proof_batches(targets: list[PackJar], batch_size: int) -> list[list[PackJar]]:
    """Chunks of targets that can share one loader process (unique mod ids)."""
    batches: list[list[PackJar]] = []
    current: list[PackJar] = []
    seen: set[str] = set()
    for jar in sorted(targets, key=lambda item: (item.mod_id.lower(), item.name)):
        key = jar.mod_id.lower()
        if key in seen:
            batches.append(current)
            current, seen = [], set()
        current.append(jar)
        seen.add(key)
        if len(current) >= max(1, batch_size):
            batches.append(current)
            current, seen = [], set()
    if current:
        batches.append(current)
    return batches


def _loader_proof_rows(batch: list[PackJar], run: RunResult) -> list[LoaderRow]:
    listed = {ident.lower() for ident in run.loaded_ids}
    # `parse_loader` writes "no Done (" when the run missed the server; a
    # substring test would read that as success.
    done = run.note.startswith("Done (")
    rows = []
    for jar in batch:
        status = "listed" if jar.mod_id.lower() in listed else "unlisted"
        rows.append(LoaderRow(jar.name, jar.mod_id, status, 0, done,
                              str(run.log.parent) if run.log else "", run.failure))
    return rows


def measure_proof(corpus: str, targets: list[PackJar], universe: list[PackJar], *,
                  jvm_binary: Path, loader: Path, libs: Path, game: Path,
                  work_root: Path, timeout: float, batch_size: int, max_runs: int
                  ) -> list[LoaderRow]:
    """Launch the real loader for each batch, splitting a batch that cannot boot."""
    index = _provider_index(universe)
    results: list[LoaderRow] = []
    runs = 0
    stamp = f"{time.strftime('%Y%m%d-%H%M%S')}-{os.getpid()}"

    def attempt(batch: list[PackJar]) -> None:
        nonlocal runs
        if not batch or runs >= max_runs:
            return
        runs += 1
        members = _closure(batch, index)
        part = Partition(Path(corpus))
        part.loader = members
        work = work_root / corpus / f"run-{stamp}-{runs:04d}"
        run = run_loader(part, binary=jvm_binary, loader=loader, libs=libs, game=game,
                         work=work, timeout=timeout)
        rows = _loader_proof_rows(batch, run)
        results.extend(rows)
        print(f"{corpus} run {runs}: {len(batch)} target(s) in {len(members)} jars, "
              f"{run.note}, {run.seconds:.1f}s [{', '.join(jar.mod_id for jar in batch)[:90]}]",
              flush=True)
        if not run.started and len(batch) > 1:
            middle = len(batch) // 2
            attempt(batch[:middle])
            attempt(batch[middle:])

    batches = plan_proof_batches(targets, batch_size)
    for chunk in batches:
        attempt(chunk)
    # A target that shared a batch with an incompatible version of a dependency
    # never gets a clean listing; give each of those one run of its own.
    listed = {(row.jar, row.mod_id) for row in results if row.listed}
    for jar in targets:
        if runs >= max_runs:
            break
        if (jar.name, jar.mod_id) not in listed:
            attempt([jar])
    return results


# ---------------------------------------------------------------------------
# Coverage output


def coverage_lines(rows: list[CoverageRow], sources: list[Path]) -> list[str]:
    union = union_rows(rows)
    sources = list(dict.fromkeys(sources))
    lines = [
        "# Horizon corpus tier coverage",
        "# native-ok: the dedicated server runs it; needs-loader: a real Fabric loader run lists",
        "# it and reaches Done; native-gap: neither tier carries it, with the cause;",
        "# out-of-scope: built for another game version, so no 26.3 tier can carry it.",
        "# Sources: " + ", ".join(path.name for path in sources),
        "",
    ]
    for corpus in sorted({row.corpus for row in rows}):
        subset = [row for row in rows if row.corpus == corpus]
        part = coverage_counts(subset)
        lines.append(f"corpus={corpus}\tjars={part['jars']}\tnative-ok={part[NATIVE_OK]}"
                     f"\tneeds-loader={part[NEEDS_LOADER]}\tnative-gap={part[NATIVE_GAP]}"
                     f"\tout-of-scope={part[OUT_OF_SCOPE]}"
                     f"\tproven={part[PROVEN]}\tpartial={part[PARTIAL]}\tunproven={part[UNPROVEN]}")
    union_counts = coverage_counts(union)
    lines.append(f"union\tdigests={union_counts['jars']}\tnative-ok={union_counts[NATIVE_OK]}"
                 f"\tneeds-loader={union_counts[NEEDS_LOADER]}\tnative-gap={union_counts[NATIVE_GAP]}"
                 f"\tout-of-scope={union_counts[OUT_OF_SCOPE]}")
    for row in sorted(rows, key=lambda item: (item.corpus, item.tier, item.jar)):
        proof = f"\tproof={row.proof}" if row.tier != NATIVE_OK else ""
        loader = f"\tloader_id={row.loader_mod_id}" if row.loader_mod_id else ""
        cause = f"\tcause={row.cause}" if row.cause else ""
        reason = f"\t{row.reason}" if row.reason else ""
        lines.append(f"{row.corpus}/{row.jar}\tmod={row.mod_id}\tnative={row.native_label}"
                     f"\ttier={row.tier}{proof}{loader}{cause}{reason}")
    conflicts = coverage_conflicts(rows)
    if conflicts:
        lines.append(f"conflicts\t{len(conflicts)}")
        for digest, group in conflicts:
            detail = ", ".join(f"{row.corpus}/{row.jar}={row.tier}" for row in group)
            lines.append(f"conflict\t{digest[:12]}\t{detail}")
    return lines


def _counts_cell(part: dict[str, int]) -> str:
    return (f"{part['jars']} | {part[NATIVE_OK]} | {part[NEEDS_LOADER]} | "
            f"{part[NATIVE_GAP]} | {part[OUT_OF_SCOPE]}")


def coverage_docs(rows: list[CoverageRow], sources: list[Path]) -> str:
    sources = list(dict.fromkeys(sources))
    union = union_rows(rows)
    total = coverage_counts(rows)
    merged = coverage_counts(union)
    lines = [
        f"_Generated by `tools/hybrid_pack.py --coverage --docs docs/java-mod-compat-status.md` "
        f"from {', '.join(f'`{path.name}`' for path in sources)}; edit the tool, not this block._",
        "",
        f"**{merged['jars']} unique jars ({total['jars']} rows across "
        f"{len({row.corpus for row in rows})} corpora): {merged[NATIVE_OK]} `native-ok`, "
        f"{merged[NEEDS_LOADER]} `needs-loader`, {merged[NATIVE_GAP]} `native-gap`, "
        f"{merged[OUT_OF_SCOPE]} out of scope.**",
        "",
        "| Corpus | Jars | `native-ok` | `needs-loader` | `native-gap` | out of scope |",
        "|---|---:|---:|---:|---:|---:|",
    ]
    for corpus in sorted({row.corpus for row in rows}):
        subset = [row for row in rows if row.corpus == corpus]
        lines.append(f"| `{corpus}` | {_counts_cell(coverage_counts(subset))} |")
    lines.append(f"| union (SHA-256) | {_counts_cell(merged)} |")
    lines += [
        "",
        f"**Loader proof.** All {total[NEEDS_LOADER] - total[UNPROVEN]} of "
        f"{total[NEEDS_LOADER]} `needs-loader` rows are demonstrated by a real Fabric loader "
        f"launch: {total[PROVEN]} reached `Done (`, {total[PARTIAL]} were listed by a launch "
        f"that did not boot (client-only code in the server role), and "
        f"{total[UNPROVEN]} have no loader evidence.",
        "",
    ]
    moved = [row for row in rows if row.native_label == NATIVE_GAP and row.tier == NEEDS_LOADER]
    if moved:
        lines.append(f"**Native refusals absorbed by the loader tier ({len(moved)} rows).** "
                     "They are `native-gap` in the server ledger; the real loader lists them, "
                     "so the tier coverage counts them as `needs-loader`.")
        lines.append("")
        lines += ["| Cause | Rows |", "|---|---:|"]
        causes: dict[str, int] = {}
        for row in moved:
            causes[row.cause] = causes.get(row.cause, 0) + 1
        for cause, count in sorted(causes.items(), key=lambda item: (-item[1], item[0])):
            lines.append(f"| {cause} | {count} |")
        lines.append("")
    out_of_scope = [row for row in rows if row.tier == OUT_OF_SCOPE]
    if out_of_scope:
        lines.append(f"**Out of scope ({len(out_of_scope)}).** Built for another game version, so no 26.3 "
                     "tier can carry them; they belong out of the corpus.")
        lines.append("")
        lines += ["| Jar | Corpus | Why |", "|---|---|---|"]
        for row in sorted(out_of_scope, key=lambda item: item.jar):
            lines.append(f"| `{row.jar}` | `{row.corpus}` | {row.reason} |")
        lines.append("")
    gaps = [row for row in rows if row.tier == NATIVE_GAP]
    if gaps:
        lines.append(f"**Uncovered rows ({len(gaps)}).**")
        lines.append("")
        lines += ["| Jar | Corpus | Cause |", "|---|---|---|"]
        for row in sorted(gaps, key=lambda item: item.jar)[:60]:
            lines.append(f"| `{row.jar}` | `{row.corpus}` | {row.reason} |")
        if len(gaps) > 60:
            lines.append(f"\n_{len(gaps) - 60} more row(s) are in the coverage report._")
        lines.append("")
    partial = [row for row in rows if row.proof == PARTIAL]
    if partial:
        lines.append(f"**Listed but not booted ({len(partial)}).** The loader discovered these "
                     "jars, but the launch did not reach `Done (`.")
        lines.append("")
        lines += ["| Jar | Corpus | Loader id |", "|---|---|---|"]
        for row in sorted(partial, key=lambda item: item.jar)[:40]:
            lines.append(f"| `{row.jar}` | `{row.corpus}` | `{row.loader_mod_id}` |")
        if len(partial) > 40:
            lines.append(f"\n_{len(partial) - 40} more row(s) are in the coverage report._")
        lines.append("")
    conflicts = coverage_conflicts(rows)
    if conflicts:
        lines.append(f"**Cross-corpus label conflicts ({len(conflicts)}).** The same bytes are "
                     "labelled differently by the two server ledgers.")
        lines.append("")
    return "\n".join(lines)


def write_coverage_docs(path: Path, block: str) -> bool:
    """Replace the coverage block in `path`; True when the file changed."""
    text = path.read_text(encoding="utf-8")
    start = text.find(COVERAGE_DOC_START)
    end = text.find(COVERAGE_DOC_END)
    if start < 0 or end < start:
        raise ValueError(f"{path}: no {COVERAGE_DOC_START} … {COVERAGE_DOC_END} markers")
    updated = text[: start + len(COVERAGE_DOC_START)] + "\n" + block.strip("\n") + "\n" + text[end:]
    if updated == text:
        return False
    path.write_text(updated, encoding="utf-8")
    return True


def _report_path() -> Path:
    return Path(__file__).resolve().parent / "real_mod_fixtures" / "LAST_CORPUS_COVERAGE.txt"


def coverage_main(args: argparse.Namespace) -> int:
    if not args.corpus:
        args.corpus = [str(CORPUS / "corpus-xl"), str(CORPUS / "mega")]
    corpora: list[tuple[str, Path]] = []
    for spec in args.corpus:
        name, _, directory = spec.partition("=")
        directory_path = Path(directory if _ else name)
        if not directory_path.is_dir():
            print(f"hybrid_pack: corpus directory does not exist: {directory_path}",
                  file=sys.stderr)
            return 2
        corpora.append((name if _ else directory_path.name, directory_path.resolve()))
    rows: dict[str, LedgerRow] = {}
    sources = []
    for ledger in args.ledger or []:
        if not ledger.is_file():
            print(f"hybrid_pack: ledger does not exist: {ledger}", file=sys.stderr)
            return 2
        rows.update(read_ledger(ledger))
        sources.append(ledger)
    evidence: list[LoaderRow] = []
    for ledger in args.loader_ledger or []:
        if not ledger.is_file():
            print(f"hybrid_pack: loader ledger does not exist: {ledger}", file=sys.stderr)
            return 2
        try:
            evidence.extend(read_loader_ledger(ledger))
        except ValueError as error:
            print(f"hybrid_pack: {error}", file=sys.stderr)
            return 2
        sources.append(ledger)

    digests: dict[Path, str] = {}
    measured = coverage_rows(corpora, rows, evidence, digests=digests)
    if args.measure_proof:
        missing = [path for path in ("jvm_binary", "loader", "libs", "game")
                   if not getattr(args, path).exists()]
        if missing:
            print("hybrid_pack: cannot measure proof, missing: " + ", ".join(missing),
                  file=sys.stderr)
            return 2
        work_root = args.proof_work or (CORPUS / "coverage-runs")
        universe_by_path: dict[Path, PackJar] = {}
        for _, directory in corpora:
            for jar in load_pack(directory, rows):
                universe_by_path[jar.path] = jar
        for extra in args.proof_universe or []:
            if not extra.is_dir():
                print(f"hybrid_pack: proof universe does not exist: {extra}", file=sys.stderr)
                return 2
            for jar in load_pack(extra, rows):
                universe_by_path[jar.path] = jar
        universe = sorted(universe_by_path.values(), key=lambda jar: jar.name)
        loaders = args.proof_loader or [args.loader]
        for name, directory in corpora:
            corpus_rows: list[LoaderRow] = []
            for loader in loaders:
                by_jar = merge_loader_evidence(evidence)

                def needs_proof(jar: PackJar) -> bool:
                    if jar.row is None or jar.row.label in (NEEDS_LOADER, NATIVE_GAP):
                        if jar.declaration.client_only:
                            return False
                        if jar.row is not None and jar.row.reason.startswith("no native entrypoint"):
                            return False
                        found = _loader_evidence(jar.name, by_jar)
                        return found is None or not found.listed
                    return False

                targets = [jar for jar in load_pack(directory, rows) if needs_proof(jar)]
                if not targets:
                    break
                print(f"{name}: {len(targets)} jar(s) need loader proof "
                      f"({loader.name})", flush=True)
                fresh = measure_proof(
                    name, targets, universe,
                    jvm_binary=args.jvm_binary, loader=loader, libs=args.libs,
                    game=args.game, work_root=work_root, timeout=args.proof_timeout,
                    batch_size=args.batch_size, max_runs=args.max_runs,
                )
                evidence.extend(fresh)
                corpus_rows.extend(fresh)
            names = {jar.name for jar in load_pack(directory, rows)}
            corpus_rows = [row for row in merge_loader_evidence(evidence).values()
                           if row.jar in names]
            if not corpus_rows:
                continue
            proof_log = work_root / name / f"{name}-proof-ledger.txt"
            proof_log.parent.mkdir(parents=True, exist_ok=True)
            with proof_log.open("w", encoding="utf-8") as stream:
                stream.write("# Horizon corpus loader proof ledger\n")
                for row in sorted(corpus_rows, key=lambda item: (item.jar, item.mod_id)):
                    stream.write(f"mod={row.mod_id}\tstatus={row.status}\tjar={row.jar}"
                                 f"\tdone={str(row.done).lower()}\trun={row.source}\n")
            sources.append(proof_log)
            print(f"{name}: proof ledger {proof_log}", flush=True)
        measured = coverage_rows(corpora, rows, evidence, digests=digests)

    union = union_rows(measured)
    payload = coverage_payload(measured, union)
    text = "\n".join(coverage_lines(measured, sources)) + "\n"
    report = args.report or _report_path()
    report.parent.mkdir(parents=True, exist_ok=True)
    report.write_text(text, encoding="utf-8")
    if args.json_output:
        print(json.dumps(payload, indent=2, sort_keys=True))
    else:
        print("\n".join(coverage_lines(measured, sources)))
        print(f"\nreport: {report}")
    if args.docs:
        try:
            changed = write_coverage_docs(args.docs, coverage_docs(measured, sources))
        except ValueError as error:
            print(f"hybrid_pack: {error}", file=sys.stderr)
            return 2
        print(f"docs: {args.docs}{'' if changed else ' (unchanged)'}")
    counts = coverage_counts(measured)
    if args.strict and (counts[NATIVE_GAP] or counts[UNPROVEN]):
        print(f"hybrid_pack: {counts[NATIVE_GAP]} native-gap and {counts[UNPROVEN]} "
              f"unproven row(s)", file=sys.stderr)
        return 1
    return 0


# ---------------------------------------------------------------------------
# Phase 2: the native score over the loader-tier rows


SCORE_DOC_START = "<!-- java-mod-native-score:start -->"
SCORE_DOC_END = "<!-- java-mod-native-score:end -->"

# What one fresh native-only run says about a jar that needed the loader.
SCORE_NATIVE = "loader-free"  # runs, and no server-side plan targets a class outside the jar
SCORE_LOADER = "loader"     # runs, but at least one server-side game-class plan still skips
SCORE_REFUSED = "refused"   # native refuses it, or the run never reached `Done (`
SCORE_SKIPPED = "skipped"   # the server role has no entrypoint to run
SCORE_MISSING = "missing"   # the jar file is not in the corpus folder
SCORE_ORDER = (SCORE_NATIVE, SCORE_LOADER, SCORE_REFUSED, SCORE_SKIPPED, SCORE_MISSING)
SCORE_MEANING = {
    SCORE_NATIVE: "runs, and no server-side plan targets a class outside the jar",
    SCORE_LOADER: "runs, but still skips at least one server-side game-class plan",
    SCORE_REFUSED: "native execution refuses it or the run did not reach `Done (`",
    SCORE_SKIPPED: "the server role has no entrypoint to run",
    SCORE_MISSING: "the jar is not in the corpus folder",
}

FIXTURES = Path(__file__).resolve().parent / "real_mod_fixtures"
DEFAULT_SCORE_LEDGERS = ("LAST_SERVER_LEDGER_223.txt", "LAST_SERVER_LEDGER_500.txt")
DEFAULT_PROOF_LEDGERS = ("LAST_LOADER_PROOF_223.txt", "LAST_LOADER_PROOF_500.txt")


@dataclass(frozen=True)
class ScoreTarget:
    """One unique jar content to run, and the first row that names it."""

    row: CoverageRow
    jar: PackJar
    path: Path


@dataclass(frozen=True)
class ScoreOutcome:
    status: str
    engine: str
    woven: int
    skipped: int
    game_plans: int
    server_plans: int
    seconds: float
    note: str


@dataclass(frozen=True)
class ScoreRow:
    corpus: str
    jar: str
    mod_id: str
    digest: str
    status: str
    engine: str
    woven: int
    skipped: int
    game_plans: int
    server_plans: int
    seconds: float
    note: str


def score_targets(corpora: list[tuple[str, Path]], rows: dict[str, LedgerRow],
                  evidence: list[LoaderRow]
                  ) -> tuple[list[ScoreTarget], list[tuple[CoverageRow, ScoreTarget]]]:
    """The loader-tier jars to run once, and every coverage row they answer for.

    The rows are `needs-loader` in the tier coverage, so the set is exactly the
    loader tier's load. Two corpora often hold the same bytes under the same
    name; one run answers both rows.
    """
    by_jar = merge_loader_evidence(evidence)
    digests: dict[Path, str] = {}
    unique: dict[str, ScoreTarget] = {}
    pairs: list[tuple[CoverageRow, ScoreTarget]] = []
    for name, directory in corpora:
        for jar in load_pack(directory, rows):
            digest = digests.get(jar.path) or file_digest(jar.path)
            digests[jar.path] = digest
            coverage = classify_jar(name, jar, digest, by_jar)
            if coverage.tier != NEEDS_LOADER:
                continue
            key = digest or f"{name}/{jar.name}"
            target = unique.get(key)
            if target is None:
                target = ScoreTarget(coverage, jar, jar.path)
                unique[key] = target
            pairs.append((coverage, target))
    return list(unique.values()), pairs


def score_key(row: CoverageRow | ScoreRow) -> str:
    return row.digest or f"{row.corpus}/{row.jar}"


def native_status(*, present: bool, engine: str, done: bool, entrypoints: int,
                  skipped: int, game_plans: int) -> str:
    """The phase-2 question, from one fresh native-only run.

    A jar is `native` when that run reached `Done (`, executed a server
    entrypoint, and either holds no server-side plan whose target is outside
    the jar or skipped nothing at all. A plan the weaver or a game hook applied
    stops being skipped, so `skipped == 0` counts as applied even while the
    static report still names the plan. Otherwise the loader still carries that
    plan, whatever share of the jar's other skips are client-side.
    """
    if not present:
        return SCORE_MISSING
    if engine in ("", "?", "refused") or not done:
        return SCORE_REFUSED
    if entrypoints <= 0:
        return SCORE_SKIPPED
    if game_plans and skipped:
        return SCORE_LOADER
    return SCORE_NATIVE


def mixin_sides(configs: list[object]) -> dict[str, str]:
    """Each mixin class's physical side from the jar's own configs.

    A class in a common `mixins` list applies on both sides; a class only in a
    `client` or `server` list applies there. The first config that names a class
    decides, matching the load-time gate's "first config that lists it" rule,
    and a later listing on the other side widens it to `both`.
    """
    sides: dict[str, str] = {}
    for config in configs:
        if not isinstance(config, dict):
            continue
        common = {str(item) for item in config.get("mixins") or ()}
        server = common | {str(item) for item in config.get("server") or ()}
        client = common | {str(item) for item in config.get("client") or ()}
        for name in server:
            if sides.get(name) == "client":
                sides[name] = "both"
            elif name not in sides:
                sides[name] = "server"
        for name in client:
            if sides.get(name) == "server":
                sides[name] = "both"
            elif name not in sides:
                sides[name] = "client"
    return sides


def plan_counts(plans: list[object], names: set[str],
                sides: dict[str, str]) -> tuple[int, int]:
    """(game plans, server-side plans) for one jar's plan report."""
    total = game = 0
    for plan in plans:
        if not isinstance(plan, dict):
            continue
        callback = str(plan.get("callback", "")).split("#", 1)[0].replace("/", ".")
        if sides.get(callback, "both") == "client":
            continue
        total += 1
        target = str(plan.get("target", ""))
        if target and f"{target}.class" not in names:
            game += 1
    return game, total


def server_plan_counts(probe: Path, jar: Path) -> tuple[int, int]:
    """(game plans, server-side plans) from `horizon-native-java --mixin-plans`.

    A game plan is a server-side plan whose target class the jar does not hold,
    so only a loader or a native game hook can apply it. Client-only mixins are
    not the loader's business on a dedicated server and are not counted, and a
    plan listed in a common `mixins` array applies on both sides.
    """
    result = subprocess.run(
        [str(probe), "--json", "--mixin-plans", str(jar)],
        capture_output=True, text=True, timeout=120, check=False,
    )
    report = json.loads(result.stdout)
    plans = report.get("plans")
    if not isinstance(plans, list):
        raise ValueError(f"{probe.name} reported no plan list for {jar.name}")
    return plan_counts(plans, set(zipfile.ZipFile(jar).namelist()),
                       mixin_sides(report.get("configs") or []))


def native_failure(text: str, jar_name: str) -> str:
    """The one line a refused native run wrote against this jar."""
    line = next((line for line in text.splitlines()
                 if "skipping " in line and jar_name in line), None)
    if line:
        return bounded(line.rsplit(f"{jar_name}:", 1)[-1].strip())
    for pattern in (r"startup failed:\s*([^\n]+)", r"refused:\s*([^\n]+)",
                    r"Metadata\([^\n]+", r"uncaught exception[^\n]+",
                    r"no native Fabric or NeoForge entrypoint[^\n]*"):
        match = re.search(pattern, text, re.I)
        if match:
            return bounded(match.group(1) if match.lastindex else match.group(0))
    errors = [line for line in text.splitlines()
              if "ERROR" in line and (jar_name in line or "Horizon:" in line)]
    return bounded(errors[-1].split("ERROR", 1)[-1].lstrip(" :")) if errors else ""


def run_native_jar(target: ScoreTarget, *, binary: Path, resources: Path, work: Path,
                   timeout: float, port: int, probe: Path,
                   game_hooks: bool = False) -> ScoreOutcome:
    """One dedicated server with one jar, the committed ledger's method."""
    command = native_command([target.jar], binary=binary, resources=resources,
                             work=work, port=port)
    for directory in (work / "game", work / "home"):
        directory.mkdir(parents=True, exist_ok=True)
    (work / "game" / "eula.txt").write_text("eula=true\n", encoding="utf-8")
    (work / "game" / "server.properties").write_text(
        f"level-type=minecraft:flat\nonline-mode=false\nserver-port={port}\n",
        encoding="utf-8")
    env = lab.game_env(
        HORIZON_HOME=work / "home",
        HORIZON_LAUNCHER_HOME=work / "launcher-home",
        XDG_DATA_HOME=work / "xdg-data",
        XDG_CONFIG_HOME=work / "xdg-config",
    )
    if game_hooks:
        env["HORIZON_JAVA_GAME_HOOKS"] = "1"
    env["TMPDIR"] = str(work / "tmp")
    (work / "tmp").mkdir(parents=True, exist_ok=True)
    text, seconds, _code, timed_out, _log = _run(
        command, work=work, env=env, timeout=timeout, stop_at_done=True)
    parsed = parse_native(text, work / "home", 1)
    report = parsed["report"].get(target.jar.name)
    engine = str(report["engine"]) if report else "refused"
    entrypoints = int(report["entrypoints"]) if report else 0
    skipped = int(report["skipped"]) if report else 0
    woven = int(report["woven"]) if report else 0
    done = "Done (" in text
    game_plans, server_plans = (server_plan_counts(probe, target.path) if report
                                else (0, 0))
    status = native_status(present=True, engine=engine, done=done,
                           entrypoints=entrypoints, skipped=skipped,
                           game_plans=game_plans)
    note = ""
    if status == SCORE_REFUSED:
        note = native_failure(text, target.jar.name)
        if not note:
            note = "hard timeout" if timed_out else (
                "started but no `Done (`" if report else "engine refused")
    elif status == SCORE_SKIPPED:
        note = "no server-role entrypoint"
    elif status == SCORE_LOADER:
        note = (f"{game_plans} of {server_plans} server-side plan(s) target a class "
                "outside the jar")
    elif skipped == 0 and server_plans:
        note = "every server-side plan applied"
    return ScoreOutcome(status, engine, woven, skipped, game_plans, server_plans,
                        round(seconds, 3), bounded(note))


def score_counts(rows: list[ScoreRow]) -> dict[str, int]:
    counts = {"jars": len(rows), "unique": 0, SCORE_NATIVE: 0, SCORE_LOADER: 0,
              SCORE_REFUSED: 0, SCORE_SKIPPED: 0, SCORE_MISSING: 0,
              "plans_skipped": 0, "game_plans": 0, "server_plans": 0}
    seen: set[str] = set()
    for row in rows:
        counts[row.status] += 1
        key = score_key(row)
        if key in seen:
            continue
        seen.add(key)
        counts["unique"] += 1
        counts["plans_skipped"] += row.skipped
        counts["game_plans"] += row.game_plans
        counts["server_plans"] += row.server_plans
    return counts


def score_lines(rows: list[ScoreRow], *, sources: list[Path],
                meta: dict[str, str], partial: bool = False) -> list[str]:
    counts = score_counts(rows)
    lines = [
        "# Horizon phase-2 native score",
        "# Each needs-loader row is run again with the native-only dedicated server.",
        "# `loader-free` means that run reached Done, executed a server entrypoint,",
        "# and no server-side plan targets a class outside the jar (the loader carries",
        "# nothing). `loader` means it runs but at least one such plan still skips.",
        f"# date={meta['date']}\tcommit={meta['commit']}\tbinary={meta['binary']}"
        f"\tgame_hooks={meta['hooks']}",
        "# Sources: " + ", ".join(path.name for path in sources),
    ]
    if partial:
        lines.append("# partial: true (--score-limit cut the target list)")
    lines.append("")
    for row in sorted(rows, key=lambda item: (SCORE_ORDER.index(item.status),
                                              item.corpus, item.jar)):
        note = f"\tnote={row.note}" if row.note else ""
        lines.append(f"mod={row.mod_id}\tjar={row.jar}\tcorpus={row.corpus}"
                     f"\tstatus={row.status}\tengine={row.engine}"
                     f"\twoven={row.woven}\tskipped={row.skipped}"
                     f"\tgame_plans={row.game_plans}\tserver_plans={row.server_plans}"
                     f"\tseconds={row.seconds:.3f}{note}")
    summary = "\t".join(f"{key}={value}" for key, value in counts.items())
    lines.append("summary\t" + summary + ("\tpartial=true" if partial else ""))
    return lines


SCORE_HISTORY_FIELDS = ("date", "commit", "binary", "hooks", "jars", "unique",
                        SCORE_NATIVE, SCORE_LOADER, SCORE_REFUSED, SCORE_SKIPPED,
                        SCORE_MISSING, "plans_skipped", "game_plans", "server_plans")


def read_score_history(path: Path) -> list[dict[str, str]]:
    if not path.is_file():
        return []
    records: list[dict[str, str]] = []
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip() or line.startswith("#"):
            continue
        values = line.split("\t")
        if len(values) != len(SCORE_HISTORY_FIELDS):
            raise ValueError(f"{path}:{number}: expected {len(SCORE_HISTORY_FIELDS)} "
                             f"columns, got {len(values)}")
        records.append(dict(zip(SCORE_HISTORY_FIELDS, values)))
    return records


def record_score(path: Path, record: dict[str, str]) -> list[dict[str, str]]:
    """Append one measurement; the same binary and counts replaces the last row."""
    records = read_score_history(path)
    signature = tuple(record[field] for field in SCORE_HISTORY_FIELDS if field != "date")
    if records:
        previous = tuple(records[-1][field] for field in SCORE_HISTORY_FIELDS
                         if field != "date")
        if previous == signature:
            records[-1] = record
        else:
            records.append(record)
    else:
        records.append(record)
    lines = ["# Horizon phase-2 native score trend",
             "# " + "\t".join(SCORE_HISTORY_FIELDS)]
    lines += ["\t".join(record[field] for field in SCORE_HISTORY_FIELDS)
              for record in records]
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return records


def score_docs(rows: list[ScoreRow], history: list[dict[str, str]],
               sources: list[Path]) -> str:
    counts = score_counts(rows)
    runs = counts[SCORE_NATIVE] + counts[SCORE_LOADER]
    lines = [
        f"_Generated by `tools/hybrid_pack.py --native-score --docs docs/java-mod-compat-status.md` "
        f"from {', '.join(f'`{path.name}`' for path in sources)}; edit the tool, not this block._",
        "",
        f"**Phase 2 score: {runs} of {counts['jars']} `needs-loader` rows "
        f"({counts['unique']} unique jars) run natively on a fresh native-only run; "
        f"{counts[SCORE_NATIVE]} are loader-free, {counts[SCORE_LOADER]} still need the loader, "
        f"{counts[SCORE_REFUSED]} refused, {counts[SCORE_SKIPPED]} skipped, "
        f"{counts[SCORE_MISSING]} missing.**",
        "",
        "| Status | Rows | What it means |",
        "|---|---:|---|",
    ]
    for status in SCORE_ORDER:
        if counts[status]:
            lines.append(f"| `{status}` | {counts[status]} | {SCORE_MEANING[status]} |")
    lines += [
        "",
        "**Trend.** One point per measurement: a rerun with the same commit, binary and "
        "counts replaces the last row instead of adding a duplicate.",
        "",
        "| Date (UTC) | Commit | Binary | Hooks | Rows | Unique | `loader-free` | still need loader | refused | plans skipped | game plans |",
        "|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for record in history[-12:]:
        lines.append(f"| {record['date']} | `{record['commit']}` | {record['binary']} | "
                     f"{record['hooks']} | {record['jars']} | {record['unique']} | "
                     f"{record[SCORE_NATIVE]} | {record[SCORE_LOADER]} | "
                     f"{record[SCORE_REFUSED]} | {record['plans_skipped']} | "
                     f"{record['game_plans']} |")
    native = [row for row in rows if row.status == SCORE_NATIVE]
    if native:
        lines += ["", f"**Loader-free ({len(native)}).** The run reached `Done (` and no "
                  "server-side plan targets a class outside the jar.", "",
                  "| Jar | Corpus | Engine | Skipped | Server plans |",
                  "|---|---|---|---:|---:|"]
        for row in sorted(native, key=lambda item: (item.corpus, item.jar))[:60]:
            lines.append(f"| `{row.jar}` | `{row.corpus}` | {row.engine} | {row.skipped} | "
                         f"{row.server_plans} |")
        if len(native) > 60:
            lines.append(f"\n_{len(native) - 60} more row(s) are in the score ledger._")
    else:
        lines += ["", "_No `needs-loader` row is loader-free yet: every one still has a "
                  "server-side plan aimed outside its own jar._"]
    loader = [row for row in rows if row.status == SCORE_LOADER]
    if loader:
        lines += ["", f"**Still needs the loader ({len(loader)}).**", "",
                  "| Jar | Corpus | Server game plans | Server plans |",
                  "|---|---|---:|---:|"]
        for row in sorted(loader, key=lambda item: (item.corpus, item.jar))[:40]:
            lines.append(f"| `{row.jar}` | `{row.corpus}` | {row.game_plans} | "
                         f"{row.server_plans} |")
        if len(loader) > 40:
            lines.append(f"\n_{len(loader) - 40} more row(s) are in the score ledger._")
    failed = [row for row in rows if row.status in (SCORE_REFUSED, SCORE_SKIPPED,
                                                    SCORE_MISSING)]
    if failed:
        lines += ["", f"**Not runnable natively ({len(failed)}).**", "",
                  "| Jar | Corpus | Status | Why |", "|---|---|---|---|"]
        for row in sorted(failed, key=lambda item: (item.status, item.corpus, item.jar))[:40]:
            lines.append(f"| `{row.jar}` | `{row.corpus}` | `{row.status}` | {row.note} |")
        if len(failed) > 40:
            lines.append(f"\n_{len(failed) - 40} more row(s) are in the score ledger._")
    return "\n".join(lines)


def write_score_docs(path: Path, block: str) -> bool:
    text = path.read_text(encoding="utf-8")
    start = text.find(SCORE_DOC_START)
    end = text.find(SCORE_DOC_END)
    if start < 0 or end < start:
        raise ValueError(f"{path}: no {SCORE_DOC_START} … {SCORE_DOC_END} markers")
    updated = text[: start + len(SCORE_DOC_START)] + "\n" + block.strip("\n") + "\n" + text[end:]
    if updated == text:
        return False
    path.write_text(updated, encoding="utf-8")
    return True


def _git_commit() -> str:
    try:
        result = subprocess.run(
            ["git", "-C", str(Path(__file__).resolve().parent.parent),
             "rev-parse", "--short=9", "HEAD"],
            capture_output=True, text=True, timeout=10, check=False)
    except (OSError, subprocess.SubprocessError):
        return "unknown"
    return result.stdout.strip() or "unknown"


def _binary_stamp(path: Path) -> str:
    try:
        return time.strftime("%Y%m%d-%H%M", time.localtime(path.stat().st_mtime))
    except OSError:
        return "missing"


def native_score_main(args: argparse.Namespace) -> int:
    if args.score_work is None:
        args.score_work = CORPUS / "native-score-runs"
    if not args.corpus:
        args.corpus = [f"corpus-xl={CORPUS / 'corpus-xl'}",
                       f"mega={CORPUS / 'mega'}"]
    corpora: list[tuple[str, Path]] = []
    for spec in args.corpus:
        name, separator, directory = spec.partition("=")
        directory_path = Path(directory if separator else name)
        if not directory_path.is_dir():
            print(f"hybrid_pack: corpus directory does not exist: {directory_path}",
                  file=sys.stderr)
            return 2
        corpora.append((name if separator else directory_path.name, directory_path.resolve()))
    ledgers = args.ledger or [FIXTURES / name for name in DEFAULT_SCORE_LEDGERS]
    proof_ledgers = args.loader_ledger or [FIXTURES / name for name in DEFAULT_PROOF_LEDGERS]
    rows: dict[str, LedgerRow] = {}
    sources: list[Path] = []
    for ledger in ledgers:
        if not ledger.is_file():
            print(f"hybrid_pack: server ledger does not exist: {ledger}", file=sys.stderr)
            return 2
        rows.update(read_ledger(ledger))
        sources.append(ledger)
    evidence: list[LoaderRow] = []
    for ledger in proof_ledgers:
        if not ledger.is_file():
            print(f"hybrid_pack: loader proof ledger does not exist: {ledger}",
                  file=sys.stderr)
            return 2
        try:
            evidence.extend(read_loader_ledger(ledger))
        except ValueError as error:
            print(f"hybrid_pack: {error}", file=sys.stderr)
            return 2
        sources.append(ledger)
    if not args.native_binary.is_file():
        print(f"hybrid_pack: native binary does not exist: {args.native_binary}",
              file=sys.stderr)
        return 2
    if not args.resources.is_dir():
        print(f"hybrid_pack: resources directory does not exist: {args.resources}",
              file=sys.stderr)
        return 2
    if not args.plan_probe.is_file():
        print(f"hybrid_pack: plan probe does not exist: {args.plan_probe}", file=sys.stderr)
        return 2
    targets, pairs = score_targets(corpora, rows, evidence)
    if not targets:
        print("hybrid_pack: no needs-loader rows to score", file=sys.stderr)
        return 1
    partial = bool(args.score_limit)
    if partial:
        targets = targets[: args.score_limit]
        keep = {score_key(target.row) for target in targets}
        pairs = [(coverage, target) for coverage, target in pairs
                 if score_key(target.row) in keep]
        if args.docs:
            print("hybrid_pack: --score-limit cannot write the docs block",
                  file=sys.stderr)
            return 2
    stamp = f"{time.strftime('%Y%m%d-%H%M%S')}-{os.getpid()}"
    outcomes: dict[str, ScoreOutcome] = {}
    for index, target in enumerate(targets):
        work = args.score_work / f"{stamp}-{index:03d}"
        print(f"[{index + 1}/{len(targets)}] {target.jar.name}", flush=True)
        outcomes[score_key(target.row)] = run_native_jar(
            target, binary=args.native_binary, resources=args.resources, work=work,
            timeout=args.score_timeout, port=args.score_port + index,
            probe=args.plan_probe, game_hooks=args.score_game_hooks)
    score_rows = [
        ScoreRow(coverage.corpus, coverage.jar, coverage.mod_id, coverage.digest,
                 outcome.status, outcome.engine, outcome.woven, outcome.skipped,
                 outcome.game_plans, outcome.server_plans, outcome.seconds,
                 outcome.note)
        for coverage, target in pairs
        for outcome in (outcomes[score_key(target.row)],)
    ]
    counts = score_counts(score_rows)
    meta = {"date": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "commit": _git_commit(), "binary": _binary_stamp(args.native_binary),
            "hooks": "on" if args.score_game_hooks else "off"}
    text = "\n".join(score_lines(score_rows, sources=sources, meta=meta,
                                 partial=partial)) + "\n"
    args.score_report.parent.mkdir(parents=True, exist_ok=True)
    args.score_report.write_text(text, encoding="utf-8")
    history_record = {field: str(counts[field]) for field in SCORE_HISTORY_FIELDS
                      if field in counts}
    history_record.update({"date": meta["date"], "commit": meta["commit"],
                           "binary": meta["binary"], "hooks": meta["hooks"]})
    history = (read_score_history(args.score_history) if partial
               else record_score(args.score_history, history_record))
    runs = counts[SCORE_NATIVE] + counts[SCORE_LOADER]
    print(f"native score: {runs}/{counts['jars']} rows "
          f"({counts['unique']} unique jars) run natively; "
          f"{counts[SCORE_NATIVE]} loader-free, "
          f"{counts[SCORE_LOADER]} still need the loader, "
          f"{counts[SCORE_REFUSED]} refused, {counts[SCORE_MISSING]} missing")
    print(f"report: {args.score_report}")
    print(f"history: {args.score_history}")
    if args.docs:
        try:
            changed = write_score_docs(args.docs, score_docs(score_rows, history, sources))
        except ValueError as error:
            print(f"hybrid_pack: {error}", file=sys.stderr)
            return 2
        print(f"docs: {args.docs}{'' if changed else ' (unchanged)'}")
    if args.strict and counts[SCORE_NATIVE] != counts["jars"]:
        print(f"hybrid_pack: {counts['jars'] - counts[SCORE_NATIVE]} needs-loader "
              "row(s) are not native yet", file=sys.stderr)
        return 1
    return 0


def self_test() -> int:
    """Classify synthetic jars and ledgers without a game or a corpus."""
    import tempfile

    with tempfile.TemporaryDirectory(prefix="hybrid-coverage-") as tmp:
        root = Path(tmp)
        pack = root / "corpus"
        pack.mkdir()
        specs = {
            "native.jar": "native-ok",
            "needs.jar": "needs-loader",
            "refused.jar": "native-gap",
            "orphan.jar": "native-gap",
            "old.jar": "native-gap",
            "bare.jar": None,
        }
        for name in specs:
            with zipfile.ZipFile(pack / name, "w") as jar:
                ident = name.removesuffix(".jar")
                jar.writestr("fabric.mod.json", json.dumps({"id": ident, "version": "1"}))
        ledger = root / "server.txt"
        ledger.write_text("".join(
            f"{name}\trecommend={label}\tid={name[:-4]}\tverdict=ran\tskipped=0\n"
            for name, label in specs.items() if label) + "summary\ttotal=4\n",
            encoding="utf-8")
        loader = root / "loader.txt"
        loader.write_text(
            "mod=needs\tversion=1\tstatus=listed\tjar=needs.jar\tmixins=3\n"
            "mod=refused\tversion=1\tstatus=listed\tjar=refused.jar\tmixins=1\n"
            "mod=native\tversion=1\tstatus=unlisted\tjar=native.jar\tmixins=0\n"
            "mod=old\tversion=1\tstatus=unlisted\tjar=old.jar\tdone=false\t"
            "error=requires minecraft 1.21.x\n"
            "summary\tmods_found=3\tmods_listed=2\tloading_count=3\tdone=true\n",
            encoding="utf-8")
        rows = read_ledger(ledger)
        evidence = read_loader_ledger(loader)
        coverage = coverage_rows([("corpus", pack)], rows, evidence,
                                 digests={path: f"digest-{path.name}" for path in pack.iterdir()})
        result = {row.jar: row for row in coverage}
        expects = {
            "native.jar": (NATIVE_OK, NOT_NEEDED),
            "needs.jar": (NEEDS_LOADER, PROVEN),
            "refused.jar": (NEEDS_LOADER, PROVEN),
            "orphan.jar": (NATIVE_GAP, UNPROVEN),
            "old.jar": (OUT_OF_SCOPE, NOT_NEEDED),
            "bare.jar": (NATIVE_GAP, UNPROVEN),
        }
        for name, (tier, proof) in expects.items():
            row = result[name]
            if (row.tier, row.proof) != (tier, proof):
                print(f"self-test: {name}: expected {tier}/{proof}, got {row.tier}/{row.proof}",
                      file=sys.stderr)
                return 1
        if "loader did not list" in result["native.jar"].reason:
            print("self-test: native-ok row asked for loader proof", file=sys.stderr)
            return 1
        counts = coverage_counts(coverage)
        if (counts[NATIVE_OK], counts[NEEDS_LOADER], counts[NATIVE_GAP],
                counts[OUT_OF_SCOPE]) != (1, 2, 2, 1):
            print(f"self-test: wrong counts {counts}", file=sys.stderr)
            return 1
        partial = read_loader_ledger(loader)[:1]
        partial = [LoaderRow(row.jar, row.mod_id, row.status, row.mixins, False, row.source)
                   for row in partial]
        proof = classify_jar("corpus", next(jar for jar in load_pack(pack, rows)
                                            if jar.name == "needs.jar"),
                             "digest-needs.jar", merge_loader_evidence(partial))
        if proof.proof != PARTIAL:
            print(f"self-test: a listing without Done is {proof.proof}, not {PARTIAL}",
                  file=sys.stderr)
            return 1
        docs = coverage_docs(coverage, [ledger, loader])
        if "needs-loader" not in docs or "native-gap" not in docs:
            print("self-test: docs table lost a tier", file=sys.stderr)
            return 1

        # --- the phase-2 scoreboard ---
        targets, pairs = score_targets([("corpus", pack)], rows, evidence)
        names = sorted(target.jar.name for target in targets)
        if names != ["needs.jar", "refused.jar"] or len(pairs) != 2:
            print(f"self-test: native score targets {names} ({len(pairs)} rows)",
                  file=sys.stderr)
            return 1
        base = dict(present=True, engine="native-qualified", done=True,
                    entrypoints=1, skipped=10, game_plans=5)
        cases = [
            (dict(base, present=False), SCORE_MISSING),
            (dict(base, engine="refused"), SCORE_REFUSED),
            (dict(base, done=False), SCORE_REFUSED),
            (dict(base, entrypoints=0), SCORE_SKIPPED),
            (base, SCORE_LOADER),
            (dict(base, skipped=0), SCORE_NATIVE),       # every server plan applied
            (dict(base, game_plans=0), SCORE_NATIVE),    # only client-side skips
            (dict(base, skipped=3, game_plans=0), SCORE_NATIVE),
        ]
        for kwargs, expected in cases:
            status = native_status(**kwargs)
            if status != expected:
                print(f"self-test: native_status({kwargs}) = {status}, not {expected}",
                      file=sys.stderr)
                return 1
        sides = mixin_sides([
            {"mixins": ["pkg.Common"], "server": ["pkg.Server"], "client": ["pkg.Client"]},
            {"client": ["pkg.Common"]},
        ])
        if sides != {"pkg.Common": "both", "pkg.Server": "server",
                     "pkg.Client": "client"}:
            print(f"self-test: mixin sides {sides}", file=sys.stderr)
            return 1
        plans = [
            {"callback": "pkg/Common#a", "target": "net/minecraft/A"},
            {"callback": "pkg/Client#b", "target": "net/minecraft/B"},
            {"callback": "pkg/Server#c", "target": "pkg/inside/Lib"},
            {"callback": "pkg/Server#d", "target": "net/minecraft/C"},
        ]
        if plan_counts(plans, {"pkg/inside/Lib.class"}, sides) != (2, 3):
            print("self-test: server-side plan count is wrong", file=sys.stderr)
            return 1
        score_rows = [ScoreRow(row.corpus, row.jar, row.mod_id, row.digest,
                               SCORE_LOADER, "native-qualified", 0, 3, 2, 4, 1.0, "")
                      for row in coverage if row.tier == NEEDS_LOADER]
        if score_counts(score_rows)[SCORE_LOADER] != 2:
            print("self-test: native score lost a loader row", file=sys.stderr)
            return 1
        history = root / "history.txt"
        first = {"date": "2026-11-06T00:00:00Z", "commit": "abc", "binary": "b",
                 "hooks": "off", "jars": "2", "unique": "2", SCORE_NATIVE: "0",
                 SCORE_LOADER: "2", SCORE_REFUSED: "0", SCORE_SKIPPED: "0",
                 SCORE_MISSING: "0", "plans_skipped": "6", "game_plans": "6",
                 "server_plans": "8"}
        record_score(history, first)
        second = dict(first, date="2026-11-06T01:00:00Z",
                      **{SCORE_NATIVE: "1", SCORE_LOADER: "1"})
        if len(record_score(history, second)) != 2:
            print("self-test: a changed score did not append to the trend", file=sys.stderr)
            return 1
        if len(record_score(history, dict(second, date="later"))) != 2:
            print("self-test: an unchanged score duplicated its trend row", file=sys.stderr)
            return 1
        score = score_docs(score_rows, read_score_history(history), [ledger, loader])
        if "Phase 2 score" not in score or "Still needs the loader" not in score:
            print("self-test: native score docs lost their block", file=sys.stderr)
            return 1
    print("hybrid_pack self-test: ok")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("pack", type=Path, nargs="?",
                        help="directory containing the pack's jars")
    parser.add_argument("--ledger", type=Path, action="append",
                        help="text ledger from java_mod_server_ledger.py (repeatable)")
    parser.add_argument("--coverage", action="store_true",
                        help="classify every jar in --corpus as native-ok, needs-loader or native-gap")
    parser.add_argument("--native-score", action="store_true", dest="native_score",
                        help="run every needs-loader row on the native-only server and "
                             "score how many no longer need the loader")
    parser.add_argument("--score-report", type=Path,
                        default=FIXTURES / "LAST_NATIVE_SCORE.txt",
                        help="phase-2 score ledger (default: tools/real_mod_fixtures/LAST_NATIVE_SCORE.txt)")
    parser.add_argument("--score-history", type=Path,
                        default=FIXTURES / "NATIVE_SCORE_HISTORY.txt",
                        help="phase-2 trend file (default: tools/real_mod_fixtures/NATIVE_SCORE_HISTORY.txt)")
    parser.add_argument("--score-work", type=Path,
                        help="scratch root for --native-score (default: ~/horizon-corpus/native-score-runs)")
    parser.add_argument("--score-timeout", type=float, default=60.0,
                        help="seconds per native score run (default: 60)")
    parser.add_argument("--score-port", type=int, default=28800,
                        help="first port a native score run binds (default: 28800)")
    parser.add_argument("--score-limit", type=int, default=0,
                        help="run only the first N unique jars (smoke test; does not write docs)")
    parser.add_argument("--score-game-hooks", action="store_true",
                        help="set HORIZON_JAVA_GAME_HOOKS=1 and measure the opt-in hook tier")
    parser.add_argument("--plan-probe", type=Path,
                        default=Path("target/fast/horizon-native-java"),
                        help="horizon-native-java used for the game-class plan count")
    parser.add_argument("--corpus", action="append", metavar="NAME=DIR",
                        help="corpus for --coverage or --native-score (repeatable; default "
                             "corpus-xl and mega)")
    parser.add_argument("--loader-ledger", type=Path, action="append",
                        help="loader ledger or proof ledger that demonstrates needs-loader rows")
    parser.add_argument("--measure-proof", action="store_true",
                        help="launch the real loader for rows that have no proof yet")
    parser.add_argument("--proof-work", type=Path,
                        help="scratch root for --measure-proof (default: ~/horizon-corpus/coverage-runs)")
    parser.add_argument("--proof-universe", type=Path, action="append",
                        help="extra jar folder whose jars may satisfy a proof run's dependencies "
                             "(repeatable; default: the --corpus folders)")
    parser.add_argument("--proof-loader", type=Path, action="append",
                        help="Fabric loader for proof runs, tried in order for unproven rows "
                             "(repeatable; default: --loader)")
    parser.add_argument("--batch-size", type=int, default=12,
                        help="targets per loader proof run (default: 12)")
    parser.add_argument("--proof-timeout", type=float, default=120.0,
                        help="seconds per loader proof run (default: 120)")
    parser.add_argument("--max-runs", type=int, default=160,
                        help="cap loader proof runs per corpus (default: 160)")
    parser.add_argument("--report", type=Path,
                        help="coverage report path (default: tools/real_mod_fixtures/LAST_CORPUS_COVERAGE.txt)")
    parser.add_argument("--docs", type=Path,
                        help="rewrite the generated coverage block in this markdown file")
    parser.add_argument("--self-test", action="store_true",
                        help="check the coverage and native-score classification against "
                             "synthetic jars and ledgers")
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
                        help="coverage: exit 1 on a conflict; native score: exit 1 while a row "
                             "still needs the loader")
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

    if args.self_test:
        return self_test()
    if args.native_score:
        for name in ("native_binary", "resources", "plan_probe", "score_work",
                     "score_report", "score_history", "docs"):
            value = getattr(args, name)
            if value is not None:
                setattr(args, name, value.resolve())
        if args.ledger:
            args.ledger = [path.resolve() for path in args.ledger]
        if args.loader_ledger:
            args.loader_ledger = [path.resolve() for path in args.loader_ledger]
        return native_score_main(args)
    if args.coverage:
        for name in ("jvm_binary", "loader", "libs", "game", "proof_work", "report", "docs"):
            value = getattr(args, name)
            if value is not None:
                setattr(args, name, value.resolve())
        if args.ledger:
            args.ledger = [path.resolve() for path in args.ledger]
        if args.loader_ledger:
            args.loader_ledger = [path.resolve() for path in args.loader_ledger]
        return coverage_main(args)
    if not args.ledger:
        parser.error("--ledger is required unless --coverage, --native-score or --self-test is used")
    if args.pack is None:
        parser.error("a pack directory is required unless --coverage, --native-score or "
                     "--self-test is used")

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

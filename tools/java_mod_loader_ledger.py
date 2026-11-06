#!/usr/bin/env python3
"""Run Fabric's real server loader against a mod corpus and write a ledger.

The runner intentionally uses one fresh process and home per bounded batch. It
never copies or modifies jars; the only output is the deterministic ledger.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import zipfile
import hashlib
import io
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import lab  # noqa: E402

# The real-mod corpus lives on disk, outside the checkout, so no pack or game jar is committed.
CORPUS = Path.home() / "horizon-corpus"
DEFAULT_MODS = CORPUS / "corpus-xl"
DEFAULT_GAME = CORPUS / "server-26.3-pinned.jar"
DEFAULT_LIBS = CORPUS / "fabric-libs-98"
DEFAULT_LOADER = DEFAULT_LIBS / "net/fabricmc/fabric-loader/0.20.1/fabric-loader-0.20.1.jar"
ASM_VERSION = "9.8"
ASM_ARTIFACTS = ("asm", "asm-analysis", "asm-commons", "asm-tree", "asm-util")
ASM98_DIR = CORPUS / "asm98"
MAVEN_ASM = "https://repo1.maven.org/maven2/org/ow2/asm/{name}/{version}/{name}-{version}.jar"


def asm_versions(libs: Path) -> set[str]:
    versions = set()
    for path in libs.rglob("asm-*.jar"):
        match = re.search(r"(?:^|/)asm(?:-(?:analysis|commons|tree|util))?-(\d+(?:\.\d+){1,2})\.jar$", str(path))
        if match:
            versions.add(match.group(1))
    return versions


def verify_asm(libs: Path) -> str:
    versions = asm_versions(libs)
    if versions == {ASM_VERSION}:
        return ASM_VERSION
    if not versions:
        raise SystemExit(f"loader libs {libs} contain no ASM jars; expected ASM {ASM_VERSION}")
    unsupported = ", ".join(sorted(versions))
    if "9.7.1" in versions or "9.7" in versions:
        reason = "ASM 9.7.1 cannot parse 26.3 class files"
    elif "9.10.1" in versions or "9.10" in versions:
        reason = "ASM 9.10.1 causes IncompatibleClassChangeError in Fabric tinyremapper"
    else:
        reason = f"only ASM {ASM_VERSION} is supported for this loader run"
    raise SystemExit(f"unsupported ASM version(s) {unsupported} in {libs}: {reason}")


def _asm98_source(name: str) -> Path:
    local = ASM98_DIR / f"{name}-{ASM_VERSION}.jar"
    if local.is_file():
        return local
    local.parent.mkdir(parents=True, exist_ok=True)
    url = MAVEN_ASM.format(name=name, version=ASM_VERSION)
    try:
        urllib.request.urlretrieve(url, local)
    except (OSError, urllib.error.URLError) as exc:
        raise SystemExit(f"could not obtain ASM {ASM_VERSION} {name}: {exc}") from exc
    return local


def prepare_libs(explicit: Path | None) -> tuple[Path, str]:
    if explicit is not None:
        return explicit, verify_asm(explicit)
    if not DEFAULT_LIBS.is_dir():
        raise SystemExit(f"known Fabric install libraries are missing: {DEFAULT_LIBS}")
    scratch = Path(tempfile.mkdtemp(prefix="horizon-loader-libs-"))
    for source in DEFAULT_LIBS.rglob("*"):
        if not source.is_file() or "org/ow2/asm/" in source.as_posix():
            continue
        target = scratch / source.relative_to(DEFAULT_LIBS)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)
    for name in ASM_ARTIFACTS:
        source = _asm98_source(name)
        target = scratch / "org/ow2/asm" / name / ASM_VERSION / source.name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)
    return scratch, verify_asm(scratch)


def sha1(path: Path) -> str:
    digest = hashlib.sha1()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _mixin_names(mixins) -> list[str]:
    """Config names from either metadata shape: string or `{"config": ...}`."""
    if isinstance(mixins, (str, dict)):
        mixins = [mixins]
    names = []
    for entry in mixins:
        if isinstance(entry, str):
            names.append(entry)
        elif isinstance(entry, dict) and isinstance(entry.get("config"), str):
            names.append(entry["config"])
    return names


def _declared_mixins(value: dict) -> list[str]:
    """Every mixin config a `fabric.mod.json` declares for any environment.

    The ledger runs a server, so `client`-only configs apply nothing; listing
    them keeps the declared set complete and lets a row explain a zero. Newer
    metadata spells an entry as `{"config": "...", "environment": ...}`.
    """
    declared = []
    for key in ("mixins", "client", "server"):
        declared += _mixin_names(value.get(key, []))
    return declared


def metadata(path: Path) -> tuple[str, str, list[str]]:
    """The mod id, version and the mixin config files its metadata declares.

    A jar can carry jar-in-jar modules (c2me ships every subsystem as one). The
    loader registers those as separate mod containers whose mixin configs are
    declared inside the nested jar, so the row for the supplied jar has to
    gather them too; otherwise a mod with real applied mixins looks empty.
    """
    try:
        with zipfile.ZipFile(path) as jar:
            value = json.loads(jar.read("fabric.mod.json"))
            declared = _declared_mixins(value)
            for name in jar.namelist():
                if not (name.startswith("META-INF/jars/") and name.endswith(".jar")):
                    continue
                try:
                    with zipfile.ZipFile(io.BytesIO(jar.read(name))) as nested:
                        declared += _declared_mixins(json.loads(nested.read("fabric.mod.json")))
                except (KeyError, ValueError, zipfile.BadZipFile, json.JSONDecodeError):
                    continue
        return str(value.get("id", path.stem)), str(value.get("version", "?")), declared
    except (OSError, KeyError, ValueError, json.JSONDecodeError):
        return path.stem, "?", []


# Mixin's verbose mode logs one line per applied mixin: the mixin's simple class
# name, the config file it came from and the class it was merged into.
MIXIN_APPLY = re.compile(r"Mixing\s+(\S+)\s+from\s+(\S+)\s+into\s+(\S+)")
# An INFO log level: Mixin's own bookkeeping lines (renames, selections) can
# mention "exception" in a class name and must not be read as failures.
INFO_LEVEL = re.compile(r"\]\s*\[[^\]]*/INFO\]")
# The failures that stop a config from reaching its target.
MIXIN_FAILURE = re.compile(
    r"(mixin apply failed|critical injection failure|mixin transformation failed"
    r"|invalidinjectionexception|mixinexception)",
    re.I,
)
# The bounded line `horizon-jvm --mixin-audit` prints for the whole launch.
MIXIN_AUDIT = re.compile(r"^mixin-audit:\s+(.+)$", re.M)


def discover(explicit: Path | None, fallback: Path, pattern: str) -> Path:
    if explicit:
        return explicit
    if fallback.exists():
        return fallback
    roots = [CORPUS]
    found = sorted(p for root in roots for p in root.rglob(pattern) if p.is_file() or p.is_dir())
    if not found:
        raise SystemExit(f"could not discover {pattern}; pass the corresponding option")
    return found[0]


def _append_child_log(output: str, launch: Path) -> str:
    """Fabric's mod listing and Mixin's application lines go to the game log."""
    log = launch / "game" / "logs" / "latest.log"
    if log.is_file():
        try:
            return output + "\n" + log.read_text(errors="replace")
        except OSError:
            pass
    return output


def run_batch(binary: Path, game: Path, loader: Path, libs: Path, jars: list[Path], timeout: int,
              mixin_audit: bool = True) -> tuple[int, str]:
    with tempfile.TemporaryDirectory(prefix="horizon-loader-") as scratch:
        # One fresh home per bounded batch: the loader run never sees a real player's folders, and
        # lab.game_env sets the audio and launcher switches every game run needs.
        env = lab.game_env(**{
            name: str(Path(scratch) / name.lower())
            for name in ("HORIZON_HOME", "HORIZON_LAUNCHER_HOME", "XDG_DATA_HOME", "XDG_CONFIG_HOME")
        })
        # A pinned launch directory: the supervisor creates exactly this one and
        # writes the game log under it, so the ledger never has to guess which
        # `/tmp/horizon-launch-*` folder came from this batch (and cannot pick up
        # a concurrent run's log).
        launch = Path(scratch) / "launch"
        env.update({
            "HORIZON_REAL_LAUNCH": "1",
            "HORIZON_LAUNCH_STOP_AT_DONE": "1",
            "HORIZON_LAUNCH_DIR": str(launch),
            "HORIZON_MINECRAFT_JAR": str(game),
            "FABRIC_LOADER_JAR": str(loader),
            "FABRIC_LIBS_DIR": str(libs),
        })
        command = [str(binary), "--real-loader", "--entry-kind", "server",
                   "--launch-game", "--accept-eula", "--loader", str(loader),
                   "--libs", str(libs), "--game-jar", str(game), "--game-version", "26.3"]
        if mixin_audit:
            # Mixin's own record: every applied mixin becomes one `Mixing ...`
            # line in the game log this runner already appends.
            command.append("--mixin-audit")
        command += [str(j) for j in jars]
        process = subprocess.Popen(command, cwd=scratch, env=env, start_new_session=True,
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        try:
            output, _ = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            output, _ = process.communicate()
            output = _append_child_log(output, launch)
            output += f"\nledger: timeout after {timeout}s\n"
            return 124, output
        return process.returncode, _append_child_log(output, launch)


def parse_log(output: str, mods: list[tuple[Path, str, str, list[str]]]) -> dict:
    listed = set()
    for line in output.splitlines():
        listed_match = re.match(r"^\s*-\s+([a-z0-9][a-z0-9_-]{1,63})(?:\s+|$)", line, re.I)
        if listed_match:
            listed.add(listed_match.group(1).lower())
    applied: list[tuple[str, str, str]] = []
    mixin_failures: list[str] = []
    for line in output.splitlines():
        if MIXIN_FAILURE.search(line):
            mixin_failures.append(line.strip())
            continue
        for match in MIXIN_APPLY.finditer(line):
            applied.append((match.group(1), match.group(2), match.group(3)))
    # Mixin's own INFO lines name classes such as `...reload_exceptions...`; they
    # are not failures. Everything below scans for failures, so drop them first.
    crash_scan = [line.strip() for line in output.splitlines()
                  if MIXIN_APPLY.search(line) is None and "Mixing " not in line
                  and "Renaming " not in line and INFO_LEVEL.search(line) is None]
    applied_by_config: dict[str, int] = {}
    for _, config, _ in applied:
        applied_by_config[config] = applied_by_config.get(config, 0) + 1
    applied_targets = {target for _, _, target in applied}
    audit_line = ""
    for match in MIXIN_AUDIT.finditer(output):
        audit_line = match.group(1).strip()
    crashes = [line for line in crash_scan if re.search(r"exception|crash|fatal|error", line, re.I)]
    rows = []
    for path, mod_id, version, mixin_configs in sorted(mods, key=lambda item: (item[1], str(item[0]))):
        mentions = [line for line in crash_scan
                    if mod_id.lower() in line.lower() and re.search(r"exception|crash|fatal|error", line, re.I)]
        error = mentions[:1]
        status = "error" if error else ("listed" if mod_id.lower() in listed else "unlisted")
        row_configs = [config for config in mixin_configs if config in applied_by_config]
        rows.append({"id": mod_id, "version": version, "jar": path.name, "status": status,
                     "mixins": sum(applied_by_config[config] for config in row_configs),
                     "mixin_configs": len(row_configs),
                     "first_exception": error[0] if error else ""})
    reasons = []
    for line in crashes:
        if line not in reasons:
            reasons.append(line)
    loading = re.findall(r"Loading\s+(\d+)\s+mods?", output, re.I)
    return {"rows": rows, "mods_found": len(rows), "mods_listed": sum(r["id"].lower() in listed for r in rows),
            "mixin_lines": [f"{config}:{mixin} -> {target}" for mixin, config, target in applied[:20]],
            "mixin_configs": [f"{config}={count}" for config, count in sorted(applied_by_config.items())],
            "mixins_applied": len(applied), "mixin_targets": len(applied_targets),
            "mixin_failures": mixin_failures[:20], "mixin_failure_count": len(mixin_failures),
            "mixin_audit": audit_line,
            "crash_lines": crashes[:20], "failure_reasons": reasons[:5],
            "loading_count": int(loading[-1]) if loading else None,
            "done": "Done (" in output, "exit": None}


def render(result: dict) -> str:
    server_digest = result.get("server_sha1", "33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c")
    lines = ["# Horizon real Fabric loader ledger", f"# Environment: ASM 9.8; Fabric loader 0.20.1; server jar sha1 {server_digest}", "# Deterministic rows sorted by mod id", ""]
    for row in result["rows"]:
        suffix = f"\tfirst_exception={row['first_exception']}" if row["first_exception"] else ""
        lines.append(f"mod={row['id']}\tversion={row['version']}\tstatus={row['status']}\tjar={row['jar']}"
                     f"\tmixins={row.get('mixins', 0)}\tmixin_configs={row.get('mixin_configs', 0)}{suffix}")
    lines += ["", f"summary\tmods_found={result['mods_found']}\tmods_listed={result['mods_listed']}"
              f"\tmixins_applied={result.get('mixins_applied', 0)}\tmixin_targets={result.get('mixin_targets', 0)}"
              f"\tmixin_failures={result.get('mixin_failure_count', 0)}"
              f"\tloading_count={result.get('loading_count') or 0}\tdone={str(result['done']).lower()}"]
    if result.get("mixin_audit"):
        lines.append("mixin_audit\t" + result["mixin_audit"])
    for config in result.get("mixin_configs", []):
        lines.append("mixin_config\t" + config)
    if result.get("bridge_exit_zero") and not result.get("loading_count"):
        lines.append("run_note\tbridge returned launch-game: exit 0 before a server log was available; current binary did not expose Loading/Done/mixin lines")
    for index, reason in enumerate(result.get("failure_reasons", []), 1):
        lines.append(f"failure_reason_{index}\t{reason}")
    for index, failure in enumerate(result.get("mixin_failures", []), 1):
        lines.append(f"mixin_failure_{index}\t{failure}")
    if result.get("mixin_lines"):
        lines.append("mixin_lines\t" + " | ".join(result["mixin_lines"]))
    if result.get("crash_lines"):
        lines.append("crash_lines\t" + " | ".join(result["crash_lines"]))
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mods-dir", type=Path, default=DEFAULT_MODS)
    parser.add_argument("--mod", action="append", type=Path, dest="extra_mods", help="additional mod jar (repeatable)")
    parser.add_argument("--binary", type=Path, default=Path("target/fast/horizon-jvm"))
    parser.add_argument("--game", type=Path)
    parser.add_argument("--loader", type=Path)
    parser.add_argument("--libs", type=Path)
    parser.add_argument("--batch-size", type=int, default=12)
    parser.add_argument("--timeout", type=int, default=90)
    parser.add_argument("--output", type=Path, default=Path("tools/real_mod_fixtures/LAST_LOADER_LEDGER.txt"))
    parser.add_argument("--json", action="store_true", help="emit JSON instead of the text ledger")
    parser.add_argument("--mixin-audit", action=argparse.BooleanOptionalAction, default=True,
                        help="run with --mixin-audit so Mixin logs every applied mixin (default: on)")
    args = parser.parse_args()
    if args.batch_size < 1 or args.timeout < 1:
        parser.error("--batch-size and --timeout must be positive")
    binary_arg = args.binary
    if binary_arg == Path("target/fast/horizon-jvm") and os.environ.get("HORIZON_JVM_BIN"):
        binary_arg = Path(os.environ["HORIZON_JVM_BIN"])
    binary = binary_arg if binary_arg.is_absolute() else Path.cwd() / binary_arg
    game_arg = args.game or (Path(os.environ["HORIZON_MINECRAFT_JAR"])
                             if os.environ.get("HORIZON_MINECRAFT_JAR") else None)
    loader_arg = args.loader or (Path(os.environ["FABRIC_LOADER_JAR"])
                                 if os.environ.get("FABRIC_LOADER_JAR") else None)
    game = discover(game_arg, DEFAULT_GAME, "server-26.3-pinned.jar")
    loader = discover(loader_arg, DEFAULT_LOADER, "fabric-loader-0.20.1.jar")
    explicit_libs = args.libs
    libs, asm_version = prepare_libs(explicit_libs)
    paths = sorted(args.mods_dir.glob("*.jar")) + sorted(args.extra_mods or [])
    if not paths:
        parser.error(f"no mod jars found in {args.mods_dir}")
    mods = [(path, *metadata(path)) for path in paths]
    aggregate = {"rows": [], "mods_found": 0, "mods_listed": 0, "mixin_lines": [], "mixin_configs": [],
                 "mixin_failures": [], "mixin_failure_count": 0, "mixin_targets": 0, "mixin_audit": "",
                 "crash_lines": [], "failure_reasons": [], "loading_count": None, "done": False,
                 "mixins_applied": 0, "bridge_exit_zero": False, "server_sha1": sha1(game),
                 "asm_version": asm_version}
    for start in range(0, len(paths), args.batch_size):
        batch_paths = paths[start:start + args.batch_size]
        code, output = run_batch(binary, game, loader, libs, batch_paths, args.timeout, args.mixin_audit)
        parsed = parse_log(output, [item for item in mods if item[0] in batch_paths])
        aggregate["rows"].extend(parsed["rows"])
        aggregate["mods_found"] += parsed["mods_found"]
        aggregate["mods_listed"] += parsed["mods_listed"]
        aggregate["mixin_lines"].extend(parsed["mixin_lines"])
        aggregate["mixin_lines"] = aggregate["mixin_lines"][:20]
        aggregate["mixins_applied"] += parsed["mixins_applied"]
        aggregate["mixin_targets"] += parsed["mixin_targets"]
        aggregate["mixin_failure_count"] += parsed["mixin_failure_count"]
        seen_configs = {line.split("=", 1)[0] for line in aggregate["mixin_configs"]}
        aggregate["mixin_configs"].extend(line for line in parsed["mixin_configs"]
                                          if line.split("=", 1)[0] not in seen_configs)
        aggregate["mixin_failures"].extend(failure for failure in parsed["mixin_failures"]
                                           if failure not in aggregate["mixin_failures"])
        aggregate["mixin_failures"] = aggregate["mixin_failures"][:20]
        if parsed["mixin_audit"]:
            aggregate["mixin_audit"] = parsed["mixin_audit"]
        aggregate["crash_lines"].extend(parsed["crash_lines"])
        aggregate["failure_reasons"].extend(reason for reason in parsed["failure_reasons"] if reason not in aggregate["failure_reasons"])
        aggregate["failure_reasons"] = aggregate["failure_reasons"][:5]
        aggregate["loading_count"] = parsed["loading_count"] or aggregate["loading_count"]
        aggregate["bridge_exit_zero"] = aggregate["bridge_exit_zero"] or "launch-game: exit 0" in output
        aggregate["done"] = aggregate["done"] or parsed["done"]
        print(f"batch {start // args.batch_size + 1}: {len(batch_paths)} mods, exit={code}, "
              f"listed={parsed['mods_listed']}, mixins={parsed['mixins_applied']}", flush=True)
    aggregate["rows"].sort(key=lambda row: (row["id"], row["jar"]))
    text = render(aggregate)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(text, encoding="utf-8")
    if args.json:
        print(json.dumps(aggregate, sort_keys=True, indent=2))
    else:
        print(text, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

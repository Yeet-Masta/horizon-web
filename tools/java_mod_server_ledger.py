#!/usr/bin/env python3
"""Run one Horizon dedicated-server process per Java mod and record outcomes.

Every row ends in one of three labels: `native-ok` (the server runs the jar, and the
plans it skips stay inside the jar), `needs-loader` (it runs, but its mixin plans
target game classes, which only a real loader applies) or `native-gap` (native
execution refuses it yet). `--table` prints the label table, and `--docs PATH`
rewrites the generated block of `docs/java-mod-compat-status.md` between its
`java-mod-ledger` markers. `--from PATH` reads an earlier text ledger instead of
starting servers, so a table can be regenerated without a jar set.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import time
import zipfile
from dataclasses import asdict, dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import lab  # noqa: E402

MAX_OUTPUT = 512 * 1024
MAX_REASON = 240
MAX_TABLE_REASON = 80

# The three answers a row can give. `native-ok` runs natively; `needs-loader` runs
# but its skipped plans target game classes the weaver cannot rewrite, so only a
# real loader applies them; `native-gap` is refused before it runs.
NATIVE_OK = "native-ok"
NEEDS_LOADER = "needs-loader"
NATIVE_GAP = "native-gap"
LABELS = (NATIVE_OK, NEEDS_LOADER, NATIVE_GAP)
# Labels earlier runs wrote; `read_ledger` normalises them.
LEGACY_LABELS = {"native": NATIVE_OK, "loader": NEEDS_LOADER}

# The generated table in docs/java-mod-compat-status.md lives between these two lines.
DOC_START = "<!-- java-mod-ledger:start -->"
DOC_END = "<!-- java-mod-ledger:end -->"

# The entrypoint kinds the dedicated server role asks a jar for, in the order
# `game/dedicated.rs` tries them. Every other Fabric entrypoint key (client,
# modmenu, preLaunch, sodium:config_api_user, controlify, ...) is a client-side
# hook the server role never queries.
SERVER_ENTRYPOINT_KINDS = frozenset({"server", "main", "neoforge"})
NEOFORGE_MOD_ANNOTATIONS = (
    b"Lnet/neoforged/fml/common/Mod;",
    b"Lnet/minecraftforge/fml/common/Mod;",
)


@dataclass(frozen=True)
class JarEntrypoints:
    """Loader entrypoints read from a jar without running it."""

    kinds: tuple[str, ...] = ()
    neoforge: bool = False
    client_side_only: bool = False

    @property
    def server(self) -> bool:
        """Whether the dedicated server role has an entrypoint to execute."""
        return self.neoforge or any(kind in SERVER_ENTRYPOINT_KINDS for kind in self.kinds)

    @property
    def available(self) -> str:
        kinds = set(self.kinds)
        if self.neoforge:
            kinds.add("neoforge")
        return ", ".join(sorted(kinds)) if kinds else "none"


@dataclass(frozen=True)
class Row:
    file: str
    id: str = "?"
    version: str = "?"
    verdict: str = "refused"
    subtype: str = ""
    engine: str = "?"
    woven: int = 0
    skipped: int = 0
    reason: str = ""
    seconds: float = 0.0
    skip_reasons: tuple[str, ...] = ()
    # One of LABELS; empty until `parse_log` classifies the row.
    recommend: str = ""


def bounded(value: str, limit: int = MAX_REASON) -> str:
    value = " ".join(value.replace("\x00", "").split())
    return value if len(value) <= limit else value[: limit - 1] + "…"


def metadata(path: Path) -> tuple[str, str]:
    try:
        with zipfile.ZipFile(path) as jar:
            try:
                obj = json.loads(jar.read("fabric.mod.json"))
                return str(obj.get("id", "?")), str(obj.get("version", "?"))
            except KeyError:
                for name in ("META-INF/neoforge.mods.toml", "META-INF/mods.toml"):
                    try:
                        text = jar.read(name).decode("utf-8", errors="replace")
                    except KeyError:
                        continue
                    ident = re.search(r"\bmodId\s*=\s*\"([^\"]+)\"", text)
                    version = re.search(r"\bversion\s*=\s*\"([^\"]+)\"", text)
                    if ident:
                        return ident.group(1), version.group(1) if version else "?"
                return "?", "?"
    except (OSError, KeyError, ValueError, zipfile.BadZipFile):
        return "?", "?"


def lenient_json_text(text: str) -> str:
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
            out.append(char)
            continue
        if char == "\\":
            escaped = True
            out.append(char)
        elif char == '"':
            in_string = False
            out.append(char)
        elif ord(char) < 0x20:
            out.append(escapes.get(char, f"\\u{ord(char):04x}"))
        else:
            out.append(char)
    return "".join(out)


def fabric_entrypoint_kinds(obj: dict) -> tuple[str, ...]:
    """The Fabric entrypoint keys that carry at least one usable entry."""
    kinds = set()
    groups = obj.get("entrypoints")
    if isinstance(groups, dict):
        for kind, group in groups.items():
            values = group if isinstance(group, list) else [group]
            if any(
                isinstance(value, str)
                or (isinstance(value, dict) and isinstance(value.get("value"), str))
                for value in values
            ):
                kinds.add(str(kind))
    return tuple(sorted(kinds))


def read_jar_entrypoints(path: Path | None) -> JarEntrypoints | None:
    """Read a jar's loader metadata; None when the jar cannot be read.

    This mirrors the native reader's rule: Fabric entrypoint keys, NeoForge
    `@Mod` classes, and `clientSideOnly` from the NeoForge/Forge TOML.
    """
    if path is None:
        return None
    try:
        with zipfile.ZipFile(path) as jar:
            names = jar.namelist()
            kinds: tuple[str, ...] = ()
            if "fabric.mod.json" in names:
                raw = jar.read("fabric.mod.json").decode("utf-8", errors="replace")
                obj = json.loads(lenient_json_text(raw))
                if not isinstance(obj, dict):
                    return None
                kinds = fabric_entrypoint_kinds(obj)
            client_side_only = False
            for name in ("META-INF/neoforge.mods.toml", "META-INF/mods.toml"):
                if name in names:
                    text = jar.read(name).decode("utf-8", errors="replace")
                    client_side_only = bool(
                        re.search(r"(?im)^\s*clientSideOnly\s*=\s*true\b", text)
                    )
                    break
            if client_side_only or any(kind in SERVER_ENTRYPOINT_KINDS for kind in kinds):
                # The server has something to execute (or the jar says it is
                # client-only); no class scan can change that verdict.
                return JarEntrypoints(kinds, False, client_side_only)
            neoforge = False
            for name in names:
                if (
                    not name.endswith(".class")
                    or name.endswith("module-info.class")
                    or name.endswith("package-info.class")
                    or name.startswith("META-INF/versions/")
                ):
                    continue
                data = jar.read(name)
                if any(marker in data for marker in NEOFORGE_MOD_ANNOTATIONS):
                    neoforge = True
                    break
            return JarEntrypoints(kinds, neoforge, client_side_only)
    except (OSError, ValueError, KeyError, EOFError, RuntimeError, zipfile.BadZipFile):
        return None


def skip_subtype(entrypoints: JarEntrypoints | None) -> str | None:
    """Classify a jar the server cannot execute: client-only, library, or neither."""
    if entrypoints is None:
        return None
    if entrypoints.client_side_only:
        return "client-only"
    if entrypoints.server:
        return None
    if entrypoints.kinds:
        return "client-only"
    return "library"


def parse_log(text: str, filename: str, *, seconds: float = 0.0, timed_out: bool = False,
              exit_code: int | None = None, jar: Path | None = None) -> Row:
    """Extract the stable fields emitted by the dedicated Java compatibility logger."""
    ident, version = "?", "?"
    engine = "?"
    match = re.search(r"engine=([^\s]+).*?\bid=([^\s]+)\s+version=([^\s]+)", text)
    if match:
        engine, ident, version = match.groups()
    else:
        match = re.search(r"\bid=([^\s]+)\s+version=([^\s]+).*?engine=([^\s]+)", text)
        if match:
            ident, version, engine = match.groups()

    woven = skipped = 0
    counts = re.search(r"mixin weave:\s*(\d+)\s+woven,\s*(\d+)\s+skipped", text)
    if counts:
        woven, skipped = map(int, counts.groups())
    else:
        counts = re.search(r"(?:^|\s)mixin:\s*(\d+)\s+woven,\s*(\d+)\s+skipped", text, re.M)
        if counts:
            woven, skipped = map(int, counts.groups())

    skip_reasons = tuple(re.findall(
        r"\b(target-outside-jar|target-missing|game-class)\b", text, re.I
    ))

    done = bool(re.search(r"Done \(", text))
    reason = ""
    for pattern in (r"skipping .*?\.jar:\s*([^\n]+)", r"startup failed:\s*([^\n]+)", r"refused:\s*([^\n]+)",
                    r"Metadata\([^\n]+", r"expected a [^\n]+"):
        failure = re.search(pattern, text, re.I)
        if failure:
            reason = bounded(failure.group(1) if failure.lastindex else failure.group(0))
            break
    if not re.search(r"no native Fabric or NeoForge entrypoint", reason, re.I):
        entrypoint_line = next((line for line in text.splitlines()
                                if re.search(r"no native Fabric or NeoForge entrypoint", line, re.I)), None)
        if entrypoint_line:
            reason = bounded(entrypoint_line.split("ERROR", 1)[-1].lstrip(" :"))
    if not reason:
        errors = [line for line in text.splitlines() if "ERROR" in line and ("Horizon:" in line or filename in line)]
        if errors:
            reason = bounded(errors[-1].split("ERROR", 1)[-1].lstrip(" :"))
    subtype = ""
    # A refused engine can still let the dedicated server reach Done: the mod
    # was skipped while the vanilla server continued, so Done is not success
    # for that row.
    if done and engine != "refused":
        verdict = "ran"
        reason = ""
    else:
        if timed_out:
            reason = reason or "hard timeout"
        else:
            reason = reason or (f"exit code {exit_code}" if exit_code not in (None, 0)
                                else ("engine refused" if engine == "refused"
                                      else "server stopped before Done"))
        entrypoint_phrase = re.search(
            r"no native Fabric or NeoForge entrypoint", reason + "\n" + text, re.I
        )
        entrypoint = re.search(
            r"no native Fabric or NeoForge entrypoint for server execution\s*"
            r"\(available:\s*([^)]*)\)", reason + "\n" + text, re.I
        )
        if entrypoint_phrase:
            # The logger bounds its line at 160 characters, which clips the
            # `(available: ...)` tail on long paths. The jar's own metadata is
            # authoritative; the log is the fallback for unreadable jars.
            entrypoints = read_jar_entrypoints(jar)
            subtype = skip_subtype(entrypoints)
            if not subtype and entrypoint:
                available = {item.strip() for item in entrypoint.group(1).split(",")}
                subtype = "client-only" if available & {"client", "modmenu"} else "library"
            if subtype:
                verdict = "skipped"
                if entrypoints is not None and "(available:" not in reason.lower():
                    reason = bounded(f"{reason} (available: {entrypoints.available})")
            else:
                verdict = "refused"
        else:
            verdict = "refused"
    row = Row(filename, ident, version, verdict, subtype, engine, woven, skipped,
              reason, round(seconds, 3), skip_reasons)
    return Row(filename, ident, version, verdict, subtype, engine, woven, skipped,
               reason, round(seconds, 3), skip_reasons, recommendation(row))


LOADER_SKIP_REASONS = {"target-outside-jar", "target-missing", "game-class"}


def recommendation(row: Row) -> str:
    """Choose the next compatibility path from the stable server outcome."""
    if row.engine.lower() in {"loader", "jvm", "knot"}:
        return NEEDS_LOADER
    if row.verdict == "skipped" and row.subtype in {"client-only", "library"}:
        return NATIVE_OK
    if row.verdict == "ran":
        loader_skips = sum(reason.lower() in LOADER_SKIP_REASONS for reason in row.skip_reasons)
        if row.skipped and loader_skips * 2 >= row.skipped and loader_skips:
            return NEEDS_LOADER
        return NATIVE_OK
    return NATIVE_GAP


def label(row: Row) -> str:
    """The row's stored label, normalised; `recommendation` when it has none."""
    stored = LEGACY_LABELS.get(row.recommend, row.recommend)
    return stored if stored in LABELS else recommendation(row)


def reason_category(row: Row) -> str:
    if row.verdict == "ran":
        return "none"
    reason = row.reason.lower()
    if "no native fabric or neoforge entrypoint" in reason:
        return "no-entrypoint"
    if "timeout" in reason:
        return "timeout"
    if "metad" in reason:
        return "metadata"
    if "invalidclass" in reason:
        return "invalid-class"
    if "startup failed" in reason:
        return "startup-failed"
    if "exit code" in reason:
        return "exit-code"
    return "other"


def summary(rows: list[Row]) -> dict[str, object]:
    categories = sorted({reason_category(row) for row in rows})
    recommendations = sorted({label(row) for row in rows}, key=LABELS.index)
    return {
        "total": len(rows),
        "ran": sum(row.verdict == "ran" for row in rows),
        "refused": sum(row.verdict == "refused" for row in rows),
        "skipped": sum(row.verdict == "skipped" for row in rows),
        "reason_categories": {category: sum(reason_category(row) == category for row in rows)
                              for category in categories},
        "recommendations": {value: sum(label(row) == value for row in rows)
                             for value in recommendations},
    }


# What each label asks a player or maintainer to do.
MEANING = {
    NATIVE_OK: "the dedicated server runs it on its own",
    NEEDS_LOADER: "it runs, but its plans target the game, so only a real loader applies them",
    NATIVE_GAP: "native execution cannot carry it yet",
}

# The clause a row adds to its label's `why`, in the generated summary table.
CAUSE_TEXT = {
    (NATIVE_OK, "ran-clean"): "ran with no plan skipped",
    (NATIVE_OK, "ran"): "ran, every skipped plan stayed inside the jar",
    (NATIVE_OK, "client-only"): "client-only (the server role has no entrypoint to run)",
    (NATIVE_OK, "library"): "libraries (nothing for the server to execute)",
    (NEEDS_LOADER, "game-plans"): "ran, but their skipped plans target the game",
    (NEEDS_LOADER, "loader-engine"): "ran through the reference loader",
    (NATIVE_GAP, "metadata"): "refused while decoding metadata",
    (NATIVE_GAP, "no-entrypoint"): "refused before a server entrypoint matched",
    (NATIVE_GAP, "invalid-class"): "refused by the verifier (opcode or stack)",
    (NATIVE_GAP, "startup-failed"): "refused in the entrypoint",
    (NATIVE_GAP, "timeout"): "did not reach `Done (` in the time bound",
    (NATIVE_GAP, "exit-code"): "exited before `Done (`",
    (NATIVE_GAP, "other"): "refused for another cause",
    (NATIVE_GAP, "none"): "refused for another cause",
}

# The per-jar table's short cause, before the ledger's own reason is appended.
GAP_TEXT = {
    "metadata": "refused while decoding metadata",
    "no-entrypoint": "refused before a server entrypoint matched",
    "invalid-class": "refused by the verifier",
    "startup-failed": "refused in the entrypoint",
    "timeout": "no `Done (` before the time bound",
    "exit-code": "exited before `Done (`",
    "other": "refused",
    "none": "refused",
}


def cause_key(row: Row) -> str:
    """The bucket a row contributes to in its label's `why`."""
    kind = label(row)
    if kind == NATIVE_OK:
        if row.verdict == "skipped" and row.subtype:
            return row.subtype
        return "ran" if row.skipped else "ran-clean"
    if kind == NEEDS_LOADER:
        return "game-plans" if row.verdict == "ran" and row.skipped else "loader-engine"
    return reason_category(row)


def why(row: Row) -> str:
    """One jar's cause, for the generated per-jar table."""
    kind = label(row)
    if kind == NEEDS_LOADER:
        return (f"ran; {row.skipped} plan(s) skipped, at least half aimed at the game"
                f" (`target-outside-jar`)"
                if row.verdict == "ran" and row.skipped
                else "ran through the reference loader")
    if kind == NATIVE_OK:
        if row.subtype == "client-only":
            return "client-only: no server-role entrypoint"
        if row.subtype == "library":
            return "library: nothing for the server to execute"
        return (f"ran; {row.skipped} plan(s) skipped, none aimed at the game"
                if row.skipped else "ran; no plan skipped")
    return GAP_TEXT[reason_category(row)]


def doc_reason(reason: str) -> str:
    """A ledger reason fit for a docs table: no local paths, no clipping noise."""
    text = re.sub(r"^[\s\]:,.…\-]+", "", reason)
    # The maintainer's corpus path is not the docs' business; the jar column names the jar.
    text = re.sub(r"(?:/[\w.+@-]+)+/[\w.+@-]*\.jar:?\s*", "", text)
    # `(entrypoint, class): cause` keeps the cause, not the entry the ledger already ran.
    text = re.sub(r"^\([^)]*\):?\s*", "", text)
    text = re.sub(r"\s{2,}", " ", text).strip()
    return text


def compat_table(rows: list[Row], *, source: str = "", per_jar_limit: int = 60) -> str:
    """The generated 'what runs native, what needs the loader, why' markdown block."""
    total = len(rows)
    causes: dict[str, dict[str, int]] = {kind: {} for kind in LABELS}
    for row in rows:
        bucket = causes[label(row)]
        bucket[cause_key(row)] = bucket.get(cause_key(row), 0) + 1
    counts = {kind: sum(causes[kind].values()) for kind in LABELS}
    head = ", ".join(f"{counts[kind]} `{kind}`" for kind in LABELS if counts[kind])
    where = f" from `{source}`" if source else ""
    lines = [
        f"_Generated by `tools/java_mod_server_ledger.py --docs docs/java-mod-compat-status.md`{where};"
        " edit the tool, not this block._",
        "",
        f"**{total} jar{'s' if total != 1 else ''}: {head}.**",
        "",
        "| Label | Jars | What it means | Why |",
        "|---|---:|---|---|",
    ]
    for kind in LABELS:
        if not counts[kind]:
            continue
        parts = [f"{count} {CAUSE_TEXT.get((kind, cause), cause)}"
                 for cause, count in sorted(causes[kind].items(), key=lambda item: (-item[1], item[0]))]
        lines.append(f"| `{kind}` | {counts[kind]} | {MEANING[kind]} | {'; '.join(parts)} |")
    rest = sorted((row for row in rows if label(row) != NATIVE_OK),
                  key=lambda row: (LABELS.index(label(row)), row.file))
    lines.extend(["", f"**Jars that are not `native-ok` ({len(rest)}).**", ""])
    if rest:
        lines.extend(["| Jar | Label | Why |", "|---|---|---|"])
        for row in rest[:per_jar_limit]:
            reason = bounded(doc_reason(row.reason), MAX_TABLE_REASON)
            if len(reason) < 12 or reason[0] in "([":
                # A path with no cause behind it: say what the ledger does know.
                reason = ""
            text = why(row)
            if reason:
                text += f": `{reason}`"
            elif label(row) == NATIVE_GAP:
                text += " (the ledger's line was clipped before the cause)"
            text = text.replace("|", "\\|")
            jar = row.file.replace("|", "\\|")
            lines.append(f"| `{jar}` | `{label(row)}` | {text} |")
        if len(rest) > per_jar_limit:
            lines.append("")
            lines.append(f"_{len(rest) - per_jar_limit} more row(s) are in the ledger, not this table._")
    else:
        lines.append("_None: every jar in this run is `native-ok`._")
    return "\n".join(lines) + "\n"


def write_docs(path: Path, block: str) -> bool:
    """Replace the marked block in `path`; True when the file changed."""
    text = path.read_text(encoding="utf-8")
    start = text.find(DOC_START)
    end = text.find(DOC_END)
    if start < 0 or end < start:
        raise ValueError(f"{path}: no {DOC_START} … {DOC_END} markers")
    updated = text[: start + len(DOC_START)] + "\n" + block.strip("\n") + "\n" + text[end:]
    if updated == text:
        return False
    path.write_text(updated, encoding="utf-8")
    return True


def read_ledger(path: Path) -> list[Row]:
    """Rows from an earlier `format_row` text ledger, labels normalised."""
    rows = []
    for number, line in enumerate(path.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
        if not line.strip() or line.startswith("summary\t"):
            continue
        fields = line.split("\t")
        if len(fields) < 2:
            continue
        values = dict(field.partition("=")[::2] for field in fields[1:])
        stored = values.get("recommend", "")
        try:
            rows.append(Row(
                fields[0],
                values.get("id", "?"),
                values.get("version", "?"),
                values.get("verdict", "refused"),
                values.get("subtype", ""),
                values.get("engine", "?"),
                int(values.get("woven", "0") or 0),
                int(values.get("skipped", "0") or 0),
                values.get("reason", ""),
                float(values.get("seconds", "0") or 0),
                (),
                LEGACY_LABELS.get(stored, stored),
            ))
        except (ValueError, IndexError) as error:
            raise ValueError(f"{path}:{number}: not a ledger row ({error})") from error
    return rows


def _kill_process(proc: subprocess.Popen[bytes]) -> None:
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


def run_one(binary: Path, resources: Path, jar: Path, *, timeout: float, port: int) -> Row:
    ident, version = metadata(jar)
    with tempfile.TemporaryDirectory(prefix="horizon-ledger-") as scratch:
        root = Path(scratch)
        game_dir = root / "game"
        home = root / "home"
        game_dir.mkdir()
        home.mkdir()
        (game_dir / "eula.txt").write_text("eula=true\n", encoding="utf-8")
        (game_dir / "server.properties").write_text(
            f"level-type=minecraft:flat\nonline-mode=false\nserver-port={port}\n", encoding="utf-8"
        )
        # Each jar runs in its own scratch homes so nothing a real player keeps (logs, options, saves)
        # is read or written; lab.game_env also sets the audio and launcher switches every game run needs.
        env = lab.game_env(
            HORIZON_HOME=home,
            XDG_DATA_HOME=root / "xdg-data",
            XDG_CONFIG_HOME=root / "xdg-config",
        )
        command = [str(binary), "--resources", str(resources), "--game-dir", str(game_dir),
                   "--home", str(home), "--java-mod", str(jar), "--dedicated", "--nogui",
                   "--port", str(port), "--world", "ledger-world"]
        start = time.monotonic()
        log_path = root / "server.log"
        with log_path.open("wb") as log:
            proc = subprocess.Popen(command, cwd=game_dir, env=env, stdout=log,
                                    stderr=subprocess.STDOUT, start_new_session=True)
            timed_out = False
            deadline = start + timeout
            while proc.poll() is None:
                if time.monotonic() >= deadline:
                    timed_out = True
                    _kill_process(proc)
                    break
                try:
                    if b"Done (" in log_path.read_bytes()[-MAX_OUTPUT:]:
                        _kill_process(proc)
                        break
                except OSError:
                    pass
                time.sleep(0.05)
        try:
            with log_path.open("rb") as log:
                log.seek(0, os.SEEK_END)
                log.seek(max(0, log.tell() - MAX_OUTPUT))
                output = log.read(MAX_OUTPUT)
        except OSError:
            output = b""
        text = output.decode("utf-8", errors="replace")
        return parse_log(text, jar.name, seconds=time.monotonic() - start,
                         timed_out=timed_out, exit_code=proc.returncode, jar=jar)


def format_row(row: Row) -> str:
    values = [row.file, f"id={row.id}", f"version={row.version}", f"verdict={row.verdict}",
              f"subtype={row.subtype}",
              f"engine={row.engine}", f"woven={row.woven}", f"skipped={row.skipped}",
              f"reason={bounded(row.reason)}", f"recommend={label(row)}",
              f"seconds={row.seconds:.3f}"]
    return "\t".join(value.replace("\t", " ") for value in values)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("mods", nargs="?", type=Path, help="directory containing mod jars")
    parser.add_argument("--binary", type=Path, default=Path("target/fast/horizon-client"))
    parser.add_argument("--resources", type=Path, default=Path(".local/horizon-client/26.3"))
    parser.add_argument("--limit", type=int, default=0, help="maximum jars (0 means all)")
    parser.add_argument("--only", help="only filenames containing this substring")
    parser.add_argument("--timeout", type=float, default=45.0, help="seconds per jar")
    parser.add_argument("--port-base", type=int, default=28000)
    parser.add_argument("--json", action="store_true", dest="json_output")
    parser.add_argument("--output", type=Path, help="also write the text ledger here")
    parser.add_argument("--from", type=Path, dest="reuse",
                        help="read an earlier text ledger instead of starting servers")
    parser.add_argument("--table", action="store_true", help="print the label table as markdown")
    parser.add_argument("--docs", type=Path,
                        help="rewrite the marked table block of this markdown file")
    parser.add_argument("--table-limit", type=int, default=60,
                        help="per-jar rows in the table (0 means all)")
    args = parser.parse_args(argv)

    if args.reuse is not None:
        if not args.reuse.is_file():
            parser.error(f"ledger does not exist: {args.reuse}")
        try:
            rows = read_ledger(args.reuse)
        except ValueError as error:
            parser.error(str(error))
        if not rows:
            parser.error(f"no rows in {args.reuse}")
        source = str(args.reuse)
    else:
        if args.mods is None:
            parser.error("a mod directory, or --from PATH")
        args.binary = args.binary.resolve()
        args.resources = args.resources.resolve()
        if not args.mods.is_dir():
            parser.error(f"mod directory does not exist: {args.mods}")
        jars = sorted((p for p in args.mods.iterdir() if p.suffix.lower() == ".jar"),
                      key=lambda p: p.name)
        if args.only:
            jars = [p for p in jars if args.only in p.name or args.only in metadata(p)[0]]
        if args.limit:
            jars = jars[: max(0, args.limit)]
        if not jars:
            parser.error("no matching .jar files")
        if not args.binary.is_file():
            parser.error(f"binary does not exist: {args.binary}")
        if not args.resources.is_dir():
            parser.error(f"resources directory does not exist: {args.resources}")

        rows = []
        for index, jar in enumerate(jars):
            print(f"[{index + 1}/{len(jars)}] {jar.name}", file=sys.stderr, flush=True)
            rows.append(run_one(args.binary, args.resources, jar, timeout=args.timeout,
                                port=args.port_base + index))
        source = str(args.mods)

    rows.sort(key=lambda row: row.file)
    counts = summary(rows)
    summary_fields = [f"{key}={value}" for key, value in counts.items()
                      if key not in {"reason_categories", "recommendations"}]
    summary_fields.extend(f"recommend_{key}={value}"
                          for key, value in counts["recommendations"].items())
    summary_fields.extend(f"reason_{key}={value}" for key, value in counts["reason_categories"].items())
    text = "\n".join([format_row(row) for row in rows] + ["summary\t" + "\t".join(summary_fields)]) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(text, encoding="utf-8")
    markdown = compat_table(rows, source=source,
                            per_jar_limit=args.table_limit if args.table_limit else len(rows) + 1)
    if args.table:
        print(markdown, end="")
    if args.docs:
        try:
            changed = write_docs(args.docs, markdown)
        except (OSError, ValueError) as error:
            parser.error(str(error))
        print(f"{'updated' if changed else 'unchanged'}: {args.docs}", file=sys.stderr)
    if args.json_output:
        print(json.dumps({"rows": [asdict(row) for row in rows], "summary": counts},
                         sort_keys=True))
    elif not (args.table or args.docs):
        print(text, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

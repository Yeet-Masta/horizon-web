#!/usr/bin/env python3
"""Run one Horizon dedicated-server process per Java mod and record outcomes."""

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


def parse_log(text: str, filename: str, *, seconds: float = 0.0, timed_out: bool = False,
              exit_code: int | None = None) -> Row:
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
        entrypoint = re.search(
            r"no native Fabric or NeoForge entrypoint for server execution\s*"
            r"\(available:\s*([^)]*)\)", reason + "\n" + text, re.I
        )
        if entrypoint:
            available = {item.strip() for item in entrypoint.group(1).split(",")}
            subtype = "client-only" if available & {"client", "modmenu"} else "library"
            verdict = "skipped"
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
        return "loader"
    if row.verdict == "skipped" and row.subtype in {"client-only", "library"}:
        return "native"
    if row.verdict == "ran":
        loader_skips = sum(reason.lower() in LOADER_SKIP_REASONS for reason in row.skip_reasons)
        if row.skipped and loader_skips * 2 >= row.skipped and loader_skips:
            return "loader"
        return "native"
    return "native-gap"


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
    recommendations = sorted({recommendation(row) for row in rows})
    return {
        "total": len(rows),
        "ran": sum(row.verdict == "ran" for row in rows),
        "refused": sum(row.verdict == "refused" for row in rows),
        "skipped": sum(row.verdict == "skipped" for row in rows),
        "reason_categories": {category: sum(reason_category(row) == category for row in rows)
                              for category in categories},
        "recommendations": {value: sum(recommendation(row) == value for row in rows)
                             for value in recommendations},
    }


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
                         timed_out=timed_out, exit_code=proc.returncode)


def format_row(row: Row) -> str:
    values = [row.file, f"id={row.id}", f"version={row.version}", f"verdict={row.verdict}",
              f"subtype={row.subtype}",
              f"engine={row.engine}", f"woven={row.woven}", f"skipped={row.skipped}",
              f"reason={bounded(row.reason)}", f"recommend={recommendation(row)}",
              f"seconds={row.seconds:.3f}"]
    return "\t".join(value.replace("\t", " ") for value in values)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mods", type=Path, help="directory containing mod jars")
    parser.add_argument("--binary", type=Path, default=Path("target/fast/horizon-client"))
    parser.add_argument("--resources", type=Path, default=Path(".local/horizon-client/26.3"))
    parser.add_argument("--limit", type=int, default=0, help="maximum jars (0 means all)")
    parser.add_argument("--only", help="only filenames containing this substring")
    parser.add_argument("--timeout", type=float, default=45.0, help="seconds per jar")
    parser.add_argument("--port-base", type=int, default=28000)
    parser.add_argument("--json", action="store_true", dest="json_output")
    parser.add_argument("--output", type=Path, help="also write the text ledger here")
    args = parser.parse_args(argv)
    args.binary = args.binary.resolve()
    args.resources = args.resources.resolve()
    if not args.mods.is_dir():
        parser.error(f"mod directory does not exist: {args.mods}")
    jars = sorted((p for p in args.mods.iterdir() if p.suffix.lower() == ".jar"), key=lambda p: p.name)
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
    print(json.dumps({"rows": [asdict(row) for row in rows], "summary": counts}, sort_keys=True)
          if args.json_output else text, end="" if args.json_output else "")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

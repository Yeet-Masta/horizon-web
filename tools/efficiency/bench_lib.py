"""Shared helpers for the efficiency suite: server, client and startup runs (README.md).

Paths are this checkout's: the prepared resources are lab.RESOURCES (`.local/horizon-client/<version>`,
horizon-prepare's output), the binaries are `target/fast` (override with HORIZON_EFFICIENCY_BIN or --bin-dir),
and every run's scratch folder, world, logs, game home and launcher home live under `.local/efficiency`
(override with HORIZON_EFFICIENCY_SCRATCH). Nothing here writes to /tmp or to a player's real folders.
"""

import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import sys
import time

TOOLS = Path(__file__).resolve().parents[1]
ROOT = TOOLS.parent
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))
import lab  # noqa: E402

EXE = ".exe" if os.name == "nt" else ""
SUITE = Path(os.environ.get("HORIZON_EFFICIENCY_SCRATCH", ROOT / ".local" / "efficiency"))
RESOURCES = lab.RESOURCES
# What horizon-prepare writes last, so its presence means the install is complete (tools/discord_presence_e2e.py).
RESOURCES_MARKER = "horizon-resources.json"

# The server settings every measured run uses: flat terrain (no worldgen to wait for), a fixed port, no
# authentication, and the same distances on both sides of a comparison.
SERVER_PROPERTIES = """online-mode=false
server-port={port}
level-type=minecraft:flat
level-name=world
view-distance=10
simulation-distance=10
spawn-protection=0
max-players=20
motd=efficiency
enable-status=true
allow-nether=true
sync-chunk-writes=true
"""

DONE = re.compile(r"Done \(([0-9.]+)s\)")

# Every threshold the smoke run knows, and its unit, in the order the summary prints them. A threshold is a
# ceiling: a run at or under it passes.
THRESHOLDS = {
    "server_startup_s": ("s", "dedicated server: wall clock until its `Done (N.NNNs)!` line"),
    "server_rss_mib": ("MiB", "dedicated server: peak resident set while it starts"),
    "client_startup_s": ("s", "client: wall clock of `horizon-ctl launch --wait` until the world is ready"),
    "client_rss_mib": ("MiB", "client: peak resident set over the run"),
    "client_p50_ms": ("ms", "client: median frame time from `horizon-ctl bench`"),
}


def prepared(resources=None):
    """Whether a prepared install is here: horizon-prepare's manifest is in the resources folder."""
    return (Path(resources or RESOURCES) / RESOURCES_MARKER).is_file()


def require_prepared(resources=None):
    path = Path(resources or RESOURCES) / RESOURCES_MARKER
    if not path.is_file():
        raise SystemExit(f"no prepared install: {path} is missing (cargo run -p horizon-prepare -- all)")


def bin_dir():
    """The folder holding horizon-client and horizon-ctl: HORIZON_EFFICIENCY_BIN, else the fast profile."""
    return Path(os.environ.get("HORIZON_EFFICIENCY_BIN") or (lab.cargo_target() / "fast"))


def binary(name, directory=None):
    path = Path(directory or bin_dir()) / f"{name}{EXE}"
    if not path.is_file():
        raise SystemExit(f"{path} is missing: cargo build --profile fast -p horizon-client "
                         "-p horizon-client-lib --bins")
    return path


def game_env(base=None, **overrides):
    """The environment for a measured run: lab.game_env's scratch folders and no audio, plus no telemetry for
    the measured game (the caller passes HORIZON_HOME and friends inside its scratch folder)."""
    env = lab.game_env(base, **overrides)
    env.setdefault("HORIZON_TELEMETRY_URL", "off")
    return env


def free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def port_open(port):
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.settimeout(0.05)
        try:
            return s.connect_ex(("127.0.0.1", port)) == 0
        except OSError:
            return False


def proc_stats(pid):
    """VmRSS in MiB and the thread count of `pid`: /proc, or `ps` where there is no /proc (macOS).

    None once the process is gone, or when this platform can report neither."""
    if pid is None:
        return None
    try:
        text = Path(f"/proc/{pid}/status").read_text()
    except OSError:
        return ps_stats(pid)
    rss = threads = None
    for line in text.splitlines():
        if line.startswith("VmRSS:"):
            rss = int(line.split()[1]) / 1024.0
        elif line.startswith("Threads:"):
            threads = int(line.split()[1])
    return {"rss_mib": rss, "threads": threads}


def ps_stats(pid):
    """proc_stats' fallback: ps reports resident memory in KiB, and has no portable thread column."""
    try:
        said = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True,
                              timeout=5).stdout.strip()
        return {"rss_mib": int(said) / 1024.0, "threads": None}
    except (OSError, ValueError, subprocess.SubprocessError):
        return None


_RUNS = [0]


def scratch(prefix):
    """A new run folder under the suite's scratch, named for what it measures."""
    SUITE.mkdir(parents=True, exist_ok=True)
    while True:
        _RUNS[0] += 1
        path = SUITE / f"{prefix}.{time.strftime('%H%M%S')}.{os.getpid()}.{_RUNS[0]}"
        try:
            path.mkdir()
            return path
        except FileExistsError:
            continue


def stop_proc(proc, wait=60.0):
    """Ask a server to stop over its console, then wait, then end its process group: True if it stopped by itself."""
    try:
        if proc.stdin and not proc.stdin.closed:
            proc.stdin.write(b"stop\n")
            proc.stdin.flush()
    except (BrokenPipeError, OSError):
        pass
    deadline = time.monotonic() + wait
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            return True
        time.sleep(0.1)
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(os.getpgid(proc.pid), sig)
        except OSError:
            pass
        for _ in range(50):
            if proc.poll() is not None:
                return False
            time.sleep(0.1)
    return False


def median(values):
    vals = sorted(v for v in values if v is not None)
    if not vals:
        return None
    n = len(vals)
    return vals[n // 2] if n % 2 else (vals[n // 2 - 1] + vals[n // 2]) / 2.0


def parse_done(text):
    """The seconds a server's `Done (N.NNNs)!` line declares, or None before it prints one."""
    if isinstance(text, bytes):
        text = text.decode("utf-8", "replace")
    match = DONE.search(text)
    return float(match.group(1)) if match else None


def write_json(path, data):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(data, indent=1, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(tmp, path)


def native_mods(count, mods_dir):
    """The first `count` jars of a folder of natively runnable jars, by file name: the same set on every run."""
    if count <= 0:
        return []
    if mods_dir is None:
        raise SystemExit("this run asks for mods: pass --mods-dir with native jars")
    folder = Path(mods_dir)
    jars = sorted(folder.glob("*.jar"), key=lambda p: p.name)
    if len(jars) < count:
        raise SystemExit(f"{folder}: {len(jars)} jars, {count} asked for")
    return [str(jar.resolve()) for jar in jars[:count]]


def measure_server(impl, run_dir, port, mods=(), *, jar=None, vanilla_home=None, heap="2G",
                   resources=None, directory=None, timeout=420.0):
    """Start one server, time it to its listening port and its Done line, sample RSS, stop it.

    `impl` is "horizon" (this checkout's binary, `--dedicated`) or "vanilla" (a Java server.jar the caller owns,
    with `--vanilla-home` its unpacked bundler folder when it needs one). The server's console is its stdin, so
    `stop` ends it the way an operator would."""
    resources = Path(resources or RESOURCES)
    directory = Path(directory or bin_dir())
    run_dir = Path(run_dir)
    run_dir.mkdir(parents=True, exist_ok=True)
    (run_dir / "eula.txt").write_text("eula=true\n", encoding="utf-8")
    (run_dir / "server.properties").write_text(SERVER_PROPERTIES.format(port=port), encoding="utf-8")

    env = game_env(HORIZON_HOME=run_dir / "home")
    if impl == "vanilla":
        if jar is None:
            raise SystemExit("the vanilla server needs --jar, your own server.jar")
        jar = Path(jar).resolve()
        if vanilla_home is not None:
            vanilla_home = Path(vanilla_home).resolve()
            for name in ("libraries", "versions"):
                link = run_dir / name
                if not link.exists():
                    os.symlink(vanilla_home / name, link)
        server_jar = run_dir / "server.jar"
        if not server_jar.exists():
            os.symlink(jar, server_jar)
        cmd = ["java", f"-Xmx{heap}", "-jar", "server.jar", "nogui"]
    elif impl == "horizon":
        # The game folder keeps the prepared resources where an install keeps them, linked rather than copied.
        local = run_dir / ".local" / "horizon-client"
        local.mkdir(parents=True, exist_ok=True)
        link = local / resources.name
        if not os.path.lexists(link):
            os.symlink(resources.resolve(), link)
        cmd = [str(directory / f"horizon-client{EXE}"), "--resources", str(resources),
               "--game-dir", str(run_dir)]
        for mod in mods:
            cmd += ["--java-mod", mod]
        cmd += ["--dedicated", "--universe", str(run_dir), "--world", "world",
                "--port", str(port), "--nogui"]
    else:
        raise SystemExit(f"unknown server {impl!r}: horizon or vanilla")

    log_path = run_dir / "server.log"
    result = {"impl": impl, "mods": len(mods), "port": port, "dir": str(run_dir),
              "wall_to_port_s": None, "wall_to_done_s": None, "declared_done_s": None,
              "rss_mib": None, "rss_peak_mib": None, "threads": None,
              "stopped_cleanly": False, "exit_code": None, "log": str(log_path)}

    t0 = time.monotonic()
    with open(log_path, "wb") as log:
        proc = subprocess.Popen([str(c) for c in cmd], cwd=run_dir, env=env, stdin=subprocess.PIPE,
                                stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            while time.monotonic() - t0 < timeout:
                stats = proc_stats(proc.pid)
                if stats and stats["rss_mib"] is not None:
                    result["rss_peak_mib"] = max(result["rss_peak_mib"] or 0.0, stats["rss_mib"])
                if proc.poll() is not None:
                    result["error"] = f"the server exited with {proc.returncode} before it was ready"
                    break
                if result["wall_to_port_s"] is None and port_open(port):
                    result["wall_to_port_s"] = time.monotonic() - t0
                declared = parse_done(log_path.read_bytes()[-65536:])
                if declared is not None:
                    result["wall_to_done_s"] = time.monotonic() - t0
                    result["declared_done_s"] = declared
                    stats = proc_stats(proc.pid)
                    if stats:
                        result["rss_mib"] = stats["rss_mib"]
                        result["rss_peak_mib"] = max(result["rss_peak_mib"] or 0.0, stats["rss_mib"] or 0.0)
                        result["threads"] = stats["threads"]
                    break
                time.sleep(0.02)
            else:
                result["error"] = f"the server was not ready within {timeout:.0f} s"
            result["stopped_cleanly"] = stop_proc(proc)
        finally:
            if proc.poll() is None:
                try:
                    os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
                except OSError:
                    pass
                proc.wait(timeout=10)
    result["exit_code"] = proc.returncode
    if result["wall_to_done_s"] is None:
        # No Done line: a listening port is the fallback clock, and the error says why.
        result["wall_to_done_s"] = result["wall_to_port_s"]
    if "error" in result:
        try:
            result["log_tail"] = "\n".join(log_path.read_text(errors="replace").splitlines()[-20:])
        except OSError:
            pass
    return result


def load_thresholds(path):
    """The ceilings a smoke run enforces, from a JSON file; a key the suite does not know is an error."""
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    unknown = sorted(set(data) - set(THRESHOLDS))
    if unknown:
        raise SystemExit(f"{path}: unknown thresholds {', '.join(unknown)}; known: {', '.join(THRESHOLDS)}")
    for key, value in data.items():
        if not isinstance(value, (int, float)) or value <= 0:
            raise SystemExit(f"{path}: {key} is a ceiling in {THRESHOLDS[key][0]}, not {value!r}")
    return data


def check_thresholds(metrics, thresholds):
    """One row per known metric: value, ceiling, and whether the run is at or under it.

    A metric missing from the run (a skipped client, a server that never started) is reported as missing and
    fails, so a broken run cannot pass as a fast one."""
    rows = []
    for key, (unit, what) in THRESHOLDS.items():
        if key not in thresholds:
            continue
        value = metrics.get(key)
        rows.append({"metric": key, "unit": unit, "what": what, "value": value,
                     "limit": thresholds[key],
                     "ok": value is not None and value <= thresholds[key]})
    return rows


def format_thresholds(rows):
    """The smoke table, for a terminal or a step summary: Markdown."""
    lines = ["| measure | value | ceiling | |", "|---|---|---|---|"]
    for row in rows:
        value = "missing" if row["value"] is None else f"{row['value']:.2f} {row['unit']}"
        lines.append(f"| {row['metric']} | {value} | {row['limit']:g} {row['unit']} | "
                     f"{'ok' if row['ok'] else '**over**'} |")
    return "\n".join(lines)


def cleanup(path):
    shutil.rmtree(path, ignore_errors=True)

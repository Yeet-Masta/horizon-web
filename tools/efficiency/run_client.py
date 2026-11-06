"""Client runs: startup to a ready world, then `horizon-ctl bench` frame times, at 0 or more native jars.

    python3 tools/efficiency/run_client.py --counts 0,3,20 --seconds 6
    python3 tools/efficiency/run_client.py --counts 5 --reps 3 --max-fps 260 --warmup 5 --append

Each run launches the game offscreen through horizon-ctl, measures the wall clock of `launch --wait` (resources
loading included), samples the process's RSS and thread count, benches `--seconds` of frames (which reply with
avg_fps, p50_ms, p95_ms and the 1% and 0.1% lows), asks `status` for the game's own memory figure and quits it.
One JSON report holds every run; `--append` adds to an earlier one.
"""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

try:
    from . import bench_lib as B
except ImportError:
    import bench_lib as B

DEFAULT_OUT = B.SUITE / "client.json"
RESOURCE_LOAD = re.compile(r"resources loaded in ([0-9.]+)s")


def wait_for_exit(pid, timeout=90.0):
    """Wait until the game process is gone; False when it had to be killed."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
        except OSError:
            return True
        time.sleep(0.2)
    return False


def game_pid(sock):
    """The game horizon-ctl started for this socket: its command line names the socket."""
    needle = f"--control-socket {sock}".encode()
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            cmd = Path(f"/proc/{entry}/cmdline").read_bytes().replace(b"\x00", b" ")
        except OSError:
            continue
        if needle in cmd and b"horizon-client" in cmd:
            return int(entry)
    return None


def run_client(count, seconds, *, mods_dir=None, directory=None, resources=None, render_distance=8,
               max_fps=None, warmup=0.0, ctl_timeout=600):
    """One launch and one bench at `count` native jars: startup seconds, RSS and the bench reply."""
    resources = Path(resources or B.RESOURCES)
    run_dir = B.scratch(f"client{count}")
    sock = run_dir / "ctl.sock"
    env = B.game_env(HORIZON_HOME=run_dir / "home")
    mods = B.native_mods(count, mods_dir)
    ctl = str(B.binary("horizon-ctl", directory))
    launch = [ctl, "--socket", str(sock), "--timeout", "600", "launch",
              "--game", str(B.binary("horizon-client", directory)),
              "--offscreen", "--wait", "--world", "Bench", "--creative",
              "--resources", str(resources), "--game-dir", str(run_dir),
              "--render-distance", str(render_distance)]
    for mod in mods:
        launch += ["--java-mod", mod]

    result = {"mods": count, "startup_s": None, "launch_rc": None, "mod_jars": mods,
              "resource_load_s": None, "bench": None, "p50_ms": None, "rss_mib": None,
              "rss_peak_mib": None, "threads": None, "status_memory": None, "quit_ok": False,
              "dir": str(run_dir), "log": str(sock.with_suffix(".log"))}

    started = time.monotonic()
    try:
        launched = subprocess.run([str(c) for c in launch], env=env, capture_output=True, text=True,
                                  timeout=ctl_timeout)
    except subprocess.TimeoutExpired:
        result.update(launch_rc=None, error=f"launch did not answer within {ctl_timeout} s")
        launched = None
    result["startup_s"] = time.monotonic() - started
    if launched is not None:
        result["launch_rc"] = launched.returncode
        if launched.returncode != 0:
            said = (launched.stdout + launched.stderr).strip().splitlines()
            result["error"] = said[-1] if said else "launch failed"

    pid = game_pid(sock)
    stats = B.proc_stats(pid)
    try:
        if launched is not None and launched.returncode == 0:
            if warmup > 0:
                time.sleep(warmup)
            if max_fps is not None:
                subprocess.run([ctl, "--socket", str(sock), "--timeout", "60", "options-set",
                                "maxFps", str(max_fps)], env=env, capture_output=True, text=True, timeout=90)
            benched = subprocess.run([ctl, "--socket", str(sock), "--timeout", "120", "bench", str(seconds)],
                                     env=env, capture_output=True, text=True, timeout=seconds + 180)
            try:
                result["bench"] = json.loads(benched.stdout)
                result["p50_ms"] = result["bench"].get("p50_ms")
            except json.JSONDecodeError:
                result["error"] = f"bench replied {benched.stdout.strip()[:200]!r}"
            status = subprocess.run([ctl, "--socket", str(sock), "--timeout", "60", "status"],
                                    env=env, capture_output=True, text=True, timeout=90)
            try:
                result["status_memory"] = json.loads(status.stdout).get("memory")
            except json.JSONDecodeError:
                pass
    finally:
        for stat in (stats, B.proc_stats(pid)):
            if stat and stat["rss_mib"] is not None:
                result["rss_peak_mib"] = max(result["rss_peak_mib"] or 0.0, stat["rss_mib"])
                result["threads"] = stat["threads"]
        if pid:
            final = B.proc_stats(pid)
            if final and final["rss_mib"] is not None:
                result["rss_mib"] = final["rss_mib"]
        result["rss_mib"] = result["rss_mib"] or result["rss_peak_mib"]
        if launched is not None and launched.returncode == 0:
            quit_run = subprocess.run([ctl, "--socket", str(sock), "--timeout", "60", "quit"],
                                      env=env, capture_output=True, text=True, timeout=120)
            result["quit_ok"] = quit_run.returncode == 0
            if pid:
                wait_for_exit(pid, 120)
        if pid:
            try:
                os.kill(pid, 9)
            except OSError:
                pass

    try:
        log = Path(result["log"]).read_text(errors="replace")
    except OSError:
        log = ""
    loads = RESOURCE_LOAD.findall(log)
    if loads:
        result["resource_load_s"] = float(loads[-1])
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--counts", default="0,3,20,74",
                        help="native jar counts to run, comma-separated (the first N of --mods-dir)")
    parser.add_argument("--seconds", type=float, default=6.0, help="how long each bench measures frames")
    parser.add_argument("--reps", type=int, default=1, help="runs per count")
    parser.add_argument("--warmup", type=float, default=0.0, help="seconds between a ready world and the bench")
    parser.add_argument("--max-fps", type=int, default=None, help="set maxFps before benching, as options-set does")
    parser.add_argument("--render-distance", type=int, default=8)
    parser.add_argument("--mods-dir", type=Path, help="a folder of natively runnable jars, by file name")
    parser.add_argument("--bin-dir", type=Path, help="where horizon-client and horizon-ctl are (default target/fast)")
    parser.add_argument("--resources", type=Path, default=B.RESOURCES)
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT)
    parser.add_argument("--append", action="store_true", help="add these runs to the report already at --out")
    args = parser.parse_args(argv)

    B.require_prepared(args.resources)
    runs = []
    if args.append and args.out.exists():
        runs = json.loads(args.out.read_text(encoding="utf-8")).get("runs", [])
    for count in [int(c) for c in args.counts.split(",") if c != ""]:
        for rep in range(args.reps):
            print(f"[client mods={count} rep {rep + 1}/{args.reps}]", flush=True)
            run = run_client(count, args.seconds, mods_dir=args.mods_dir, directory=args.bin_dir,
                             resources=args.resources, render_distance=args.render_distance,
                             max_fps=args.max_fps, warmup=args.warmup)
            run["rep"] = rep + 1
            bench = run["bench"] or {}
            print(f"  startup={run['startup_s']:.2f}s rss={run['rss_mib']} thr={run['threads']} "
                  f"fps={bench.get('avg_fps')} p50={bench.get('p50_ms')} p95={bench.get('p95_ms')} "
                  f"low1={bench.get('low_1pct_fps')} rc={run['launch_rc']} quit={run['quit_ok']}", flush=True)
            runs.append(run)
            B.write_json(args.out, {"render_distance": args.render_distance, "max_fps": args.max_fps,
                                    "seconds": args.seconds, "runs": runs})
    print(json.dumps({"out": str(args.out), "n": len(runs)}))
    return 1 if any("error" in run for run in runs) else 0


if __name__ == "__main__":
    sys.exit(main())

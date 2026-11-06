"""The perf smoke: a few measured runs against the ceilings of thresholds.json, and a failing exit code above them.

    python3 tools/efficiency/smoke.py
    python3 tools/efficiency/smoke.py --only server --runs 5 --out .local/efficiency/smoke.json
    python3 tools/efficiency/smoke.py --thresholds my-runner.json     # a machine with its own baseline

It measures the dedicated server's startup and peak RSS, the client's startup, peak RSS and median frame time,
and compares each with its ceiling: startup, RSS and p50. Without a prepared install it prints a notice and exits
0, so a job on a machine that has no game files skips instead of failing (--require-install makes that a failure).
The report is JSON, written under .local/efficiency and appended to the job summary when CI sets one.
"""

import argparse
import json
import os
from pathlib import Path
import platform
import sys
import time

try:
    from . import bench_lib as B
    from .run_client import run_client
except ImportError:
    import bench_lib as B
    from run_client import run_client

HERE = Path(__file__).resolve().parent
DEFAULT_THRESHOLDS = HERE / "thresholds.json"
DEFAULT_OUT = B.SUITE / "smoke.json"


def metrics_of(server_runs, client_run):
    """The smoke's figures: medians over the server runs, one client run (the noisiest figures stay out)."""
    server_ok = [run for run in server_runs if "error" not in run]
    client_bench = (client_run or {}).get("bench") or {}
    return {
        "server_startup_s": B.median([run["wall_to_done_s"] for run in server_ok]),
        "server_rss_mib": max((run["rss_peak_mib"] for run in server_ok
                               if run["rss_peak_mib"] is not None), default=None),
        "client_startup_s": (client_run or {}).get("startup_s") if "error" not in (client_run or {}) else None,
        "client_rss_mib": (client_run or {}).get("rss_peak_mib") if "error" not in (client_run or {}) else None,
        "client_p50_ms": client_bench.get("p50_ms"),
    }


def measured_thresholds(path, only):
    """The ceilings of the sides this run measures: `--only server` does not judge a client it did not start."""
    ceilings = B.load_thresholds(path)
    if only:
        other = "client_" if only == "server" else "server_"
        ceilings = {key: value for key, value in ceilings.items() if not key.startswith(other)}
    return ceilings


def summary_markdown(report):
    """The step summary: what was measured where, the table, and anything that failed."""
    rows = B.format_thresholds(report["checks"])
    lines = [f"### Perf smoke ({report['machine']['system']}, {report['machine']['cpus']} CPUs)", "",
             rows, ""]
    for run in report["server_runs"]:
        if "error" in run:
            lines.append(f"- server run {run['run']}: {run['error']}")
            for line in (run.get("log_tail") or "").splitlines()[-5:]:
                lines.append(f"  `{line}`")
    client = report["client_run"]
    if client and "error" in client:
        lines.append(f"- client run: {client['error']}")
    return "\n".join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--only", choices=["server", "client"], help="measure one side only (the default is both)")
    parser.add_argument("--runs", type=int, default=3, help="dedicated server runs; the median is the figure")
    parser.add_argument("--seconds", type=float, default=8.0, help="how long the client's bench measures frames")
    parser.add_argument("--warmup", type=float, default=5.0, help="seconds between a ready world and the bench")
    parser.add_argument("--render-distance", type=int, default=8)
    parser.add_argument("--thresholds", type=Path,
                        default=Path(os.environ.get("HORIZON_EFFICIENCY_THRESHOLDS", DEFAULT_THRESHOLDS)))
    parser.add_argument("--bin-dir", type=Path, help="where the binaries are (default target/fast)")
    parser.add_argument("--resources", type=Path, default=B.RESOURCES)
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT)
    parser.add_argument("--require-install", action="store_true",
                        help="fail, rather than skip, when no prepared install is here")
    args = parser.parse_args(argv)

    report = {
        "kind": "horizon-efficiency-smoke",
        "date": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "machine": {"system": platform.system(), "machine": platform.machine(),
                    "cpus": os.cpu_count(), "loadavg": list(os.getloadavg())},
        "settings": {"runs": args.runs, "seconds": args.seconds, "warmup": args.warmup,
                     "render_distance": args.render_distance, "only": args.only},
        "thresholds": measured_thresholds(args.thresholds, args.only), "checks": [], "metrics": {},
        "server_runs": [], "client_run": None, "skipped": None,
    }

    if not B.prepared(args.resources):
        reason = f"no prepared install in {args.resources}: run cargo run -p horizon-prepare -- all"
        if args.require_install:
            print(f"error: {reason}", file=sys.stderr)
            return 1
        report["skipped"] = reason
        B.write_json(args.out, report)
        print(f"notice: {reason}; skipping the perf smoke")
        summary = os.environ.get("GITHUB_STEP_SUMMARY")
        if summary:
            with open(summary, "a", encoding="utf-8") as out:
                out.write(f"### Perf smoke: skipped\n\n{reason}\n")
        return 0

    B.require_prepared(args.resources)

    if args.only != "client":
        for i in range(args.runs):
            port = B.free_port()
            print(f"[server run {i + 1}/{args.runs}] port {port}", flush=True)
            run = B.measure_server("horizon", B.scratch(f"smoke-server{i}"), port, (), resources=args.resources,
                                   directory=args.bin_dir)
            run["run"] = i + 1
            state = run.get("error") or (f"done={run['wall_to_done_s']:.2f}s "
                                         f"rss={run['rss_peak_mib']:.0f}MiB")
            print(f"  {state}", flush=True)
            report["server_runs"].append(run)

    if args.only != "server":
        print("[client run]", flush=True)
        client = run_client(0, args.seconds, directory=args.bin_dir, resources=args.resources,
                            render_distance=args.render_distance, max_fps=260, warmup=args.warmup)
        report["client_run"] = client
        bench = client["bench"] or {}
        print(f"  startup={client['startup_s']:.2f}s rss={client['rss_peak_mib']:.0f}MiB "
              f"p50={bench.get('p50_ms')} rc={client['launch_rc']}", flush=True)

    metrics = metrics_of(report["server_runs"], report["client_run"])
    report["metrics"] = metrics
    report["checks"] = B.check_thresholds(metrics, report["thresholds"])
    passed = all(row["ok"] for row in report["checks"])
    report["passed"] = passed
    B.write_json(args.out, report)

    print()
    print(B.format_thresholds(report["checks"]))
    print(f"report: {args.out}")
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as out:
            out.write(summary_markdown(report) + "\n")
    if not passed:
        over = [row["metric"] for row in report["checks"] if not row["ok"]]
        print(f"over the ceiling: {', '.join(over)}", file=sys.stderr)
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())

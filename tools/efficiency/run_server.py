"""Server startup runs: this checkout's dedicated server against a vanilla 26.3 server.jar you own.

    python3 tools/efficiency/run_server.py --only horizon --runs 3
    python3 tools/efficiency/run_server.py --jar ~/mc/server.jar --vanilla-home ~/horizon-corpus/vanilla-libs

Each run gets a fresh flat world, a free port and a scratch folder; it is timed to its listening port and to its
`Done (N.NNNs)!` line, its RSS and thread count are sampled while it starts, and it is stopped through its console.
The report holds the medians next to every run. Vanilla needs a warm bundler folder (`--vanilla-home`: the
libraries and versions an `--initSettings` run leaves) so the comparison is not a library unpack.
"""

import argparse
import json
import sys
from pathlib import Path

try:
    from . import bench_lib as B
except ImportError:
    import bench_lib as B

DEFAULT_OUT = B.SUITE / "server.json"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--runs", type=int, default=3, help="runs per implementation")
    parser.add_argument("--only", choices=["vanilla", "horizon"], help="one side only (the default is both)")
    parser.add_argument("--jar", type=Path, help="your own vanilla server.jar (nothing is downloaded)")
    parser.add_argument("--vanilla-home", type=Path,
                        help="a folder holding the bundler's libraries/ and versions/, linked into each run")
    parser.add_argument("--mods-dir", type=Path, help="a folder of natively runnable jars, by file name")
    parser.add_argument("--mods", type=int, default=0, help="how many of them the Horizon runs load")
    parser.add_argument("--bin-dir", type=Path, help="where horizon-client is (default target/fast)")
    parser.add_argument("--resources", type=Path, default=B.RESOURCES)
    parser.add_argument("--timeout", type=float, default=420.0, help="seconds a server may take to be ready")
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT)
    args = parser.parse_args(argv)

    if args.only != "vanilla":
        B.require_prepared(args.resources)
    impls = [args.only] if args.only else ["vanilla", "horizon"]
    if "vanilla" in impls and args.jar is None:
        raise SystemExit("vanilla needs --jar, your own server.jar")
    mods = B.native_mods(args.mods, args.mods_dir)

    results = []
    for impl in impls:
        for i in range(args.runs):
            run_dir = B.scratch(f"server-{impl}{i}")
            port = B.free_port()
            print(f"[{impl} run {i + 1}/{args.runs}] port {port} dir {run_dir}", flush=True)
            run = B.measure_server(impl, run_dir, port, mods if impl == "horizon" else (),
                                   jar=args.jar, vanilla_home=args.vanilla_home, resources=args.resources,
                                   directory=args.bin_dir, timeout=args.timeout)
            run["run"] = i + 1
            print(f"  port={run['wall_to_port_s']} done={run['wall_to_done_s']} "
                  f"declared={run['declared_done_s']} rss={run['rss_mib']} peak={run['rss_peak_mib']} "
                  f"thr={run['threads']} clean={run['stopped_cleanly']} rc={run['exit_code']}", flush=True)
            results.append(run)
            B.write_json(args.out, {"runs": results, "medians": medians(results)})

    print(json.dumps({"out": str(args.out), "n": len(results), "medians": medians(results)}))
    return 1 if any("error" in run for run in results) else 0


def medians(runs):
    """Per implementation, the median of each startup figure over its runs."""
    out = {}
    for impl in ("vanilla", "horizon"):
        side = [run for run in runs if run["impl"] == impl]
        if side:
            out[impl] = {key: B.median([run.get(key) for run in side])
                         for key in ("wall_to_port_s", "wall_to_done_s", "declared_done_s",
                                     "rss_mib", "rss_peak_mib", "threads")}
    return out


if __name__ == "__main__":
    sys.exit(main())

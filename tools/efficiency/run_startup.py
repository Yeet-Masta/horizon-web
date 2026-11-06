"""Startup scaling: the dedicated server's time to ready and memory with 0, N and M native jars loaded.

    python3 tools/efficiency/run_startup.py --counts 0,25,50 --mods-dir ~/jars/native --runs 2

`--mods-dir` holds natively runnable jars; the first `count` of them by file name are passed as `--java-mod`, so
the same set is measured on every run. Each run is a fresh flat world on a free port, as run_server.py's are.
"""

import argparse
import json
import sys
from pathlib import Path

try:
    from . import bench_lib as B
except ImportError:
    import bench_lib as B

DEFAULT_OUT = B.SUITE / "startup.json"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--counts", default="0,25,50,74",
                        help="native jar counts, comma-separated (the first N of --mods-dir)")
    parser.add_argument("--runs", type=int, default=1, help="runs per count")
    parser.add_argument("--mods-dir", type=Path, help="a folder of natively runnable jars, by file name")
    parser.add_argument("--bin-dir", type=Path, help="where horizon-client is (default target/fast)")
    parser.add_argument("--resources", type=Path, default=B.RESOURCES)
    parser.add_argument("--timeout", type=float, default=600.0, help="seconds a server may take to be ready")
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT)
    args = parser.parse_args(argv)

    B.require_prepared(args.resources)
    results = []
    for count in [int(c) for c in args.counts.split(",") if c != ""]:
        mods = B.native_mods(count, args.mods_dir)
        for i in range(args.runs):
            run_dir = B.scratch(f"startup-mods{count}")
            port = B.free_port()
            print(f"[dedicated mods={count} run {i + 1}/{args.runs}] port {port}", flush=True)
            run = B.measure_server("horizon", run_dir, port, mods, resources=args.resources,
                                   directory=args.bin_dir, timeout=args.timeout)
            run["run"] = i + 1
            run["count"] = count
            print(f"  port={run['wall_to_port_s']} done={run['wall_to_done_s']} "
                  f"rss={run['rss_mib']} peak={run['rss_peak_mib']} thr={run['threads']} "
                  f"clean={run['stopped_cleanly']} rc={run['exit_code']}", flush=True)
            results.append(run)
            B.write_json(args.out, {"runs": results})

    print(json.dumps({"out": str(args.out), "n": len(results)}))
    return 1 if any("error" in run for run in results) else 0


if __name__ == "__main__":
    sys.exit(main())

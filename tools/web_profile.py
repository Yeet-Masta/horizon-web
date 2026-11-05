#!/usr/bin/env python3
"""Where the web build's frames go (web/README.md, "Profiling").

Serves a built page (web/dist, or --dist), opens it in Chromium, starts a new world and plays it
for --seconds, flying forward so terrain keeps loading (what most stutters come with; --still
stays put), then reports:

- frame times as the page sees them (requestAnimationFrame): the median, the slow tail, the
  dips over 33/50/100 ms and when they came;
- the game's own figures (`FrameStats`, as `horizon-ctl frame-stats` prints them natively): CPU
  and GPU time per frame, and the frame's phases on average and in its slowest 1%;
- the slow frames' named steps (`HORIZON_SPIKES`): which steps the dips spent their time in;
- the page's thread from a CPU profile: the functions it spent its time in (own time, and time
  inside them with what they call), the wasm module's by name. A call out to the browser (a
  WebGPU call, a canvas, a message) counts in the module's JavaScript glue for it
  (`__wbg_...`), so the glue's own time is the browser's work for those calls.

    python3 tools/web_profile.py --resources .local/horizon-web-resources.zip
    python3 tools/web_profile.py --resources R --channel chrome     # the installed Chrome, its GPU
    python3 tools/web_profile.py --resources R --dist D --out profile.json

Without --channel it uses Playwright's Chromium on SwiftShader (no GPU): its CPU profile holds,
its GPU times don't. A build whose module lost its names (wasm-opt strips them) shows addresses.
Needs Playwright for Python (pip install playwright; python -m playwright install chromium).
"""
import argparse
import collections
import json
import pathlib
import re
import socket
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
SWIFTSHADER = [
    "--enable-unsafe-webgpu", "--use-webgpu-adapter=swiftshader", "--ignore-gpu-blocklist",
    "--enable-unsafe-swiftshader", "--disable-features=Vulkan",
]
GPU = ["--enable-unsafe-webgpu", "--ignore-gpu-blocklist"]
COMMON = ["--autoplay-policy=no-user-gesture-required"]
# requestAnimationFrame's timestamps, kept while `__rec` is on.
RAF = """
globalThis.__rafs = []; let recording = false;
globalThis.__rec = (on) => { recording = on; if (on) globalThis.__rafs = []; };
const tick = (t) => { if (recording) globalThis.__rafs.push(t); requestAnimationFrame(tick); };
requestAnimationFrame(tick);
"""
SLOW = re.compile(r"slow frame ([\d.]+) ms[^(]*\([^)]*\): (.*)$")
SPAN = re.compile(r"^(.+?) ([\d.]+)(?:/([\d.]+))?$")
HASH = re.compile(r"\[[0-9a-f]{16}\]")


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def percentile(sorted_values, p):
    if not sorted_values:
        return None
    i = min(len(sorted_values) - 1, max(0, int(len(sorted_values) * p + 0.5) - 1))
    return sorted_values[i]


def clean(name):
    """A wasm function's name without the module prefix and crate hashes."""
    name = HASH.sub("", name or "(anonymous)")
    return name.removeprefix("horizon_client.wasm.")


def frame_report(stamps):
    gaps = [b - a for a, b in zip(stamps, stamps[1:])]
    s = sorted(gaps)
    worst = sorted(((g, stamps[i + 1] - stamps[0]) for i, g in enumerate(gaps)), reverse=True)[:10]
    return {
        "frames": len(gaps),
        "seconds": (stamps[-1] - stamps[0]) / 1000 if len(stamps) > 1 else 0,
        "p50_ms": percentile(s, 0.5),
        "p95_ms": percentile(s, 0.95),
        "p99_ms": percentile(s, 0.99),
        "max_ms": s[-1] if s else None,
        "over_33ms": sum(g > 33.4 for g in gaps),
        "over_50ms": sum(g > 50 for g in gaps),
        "over_100ms": sum(g > 100 for g in gaps),
        "worst": [{"ms": round(g, 1), "at_s": round(at / 1000, 1)} for g, at in worst],
    }


def spike_report(lines):
    """The slow-frame lines' steps: in how many slow frames each came, and its time there."""
    steps = collections.defaultdict(lambda: {"frames": 0, "ms": 0.0, "cpu_ms": 0.0, "max_ms": 0.0})
    frames = []
    for line in lines:
        m = SLOW.search(line)
        if not m:
            continue
        frames.append((float(m.group(1)), line[line.index("slow frame"):]))
        for part in m.group(2).split(", "):
            sm = SPAN.match(part.strip())
            if not sm:
                continue
            name, ms = sm.group(1), float(sm.group(2))
            cpu = float(sm.group(3)) if sm.group(3) else ms
            st = steps[name]
            st["frames"] += 1
            st["ms"] += ms
            st["cpu_ms"] += cpu
            st["max_ms"] = max(st["max_ms"], ms)
    ranked = sorted(steps.items(), key=lambda kv: -kv[1]["ms"])
    return {
        "slow_frames": len(frames),
        "steps": [{"step": k, **{x: round(y, 1) if isinstance(y, float) else y for x, y in v.items()}}
                  for k, v in ranked],
        "slowest": [line for _, line in sorted(frames, reverse=True)[:8]],
    }


def cpu_report(profile, top):
    """Own time and inclusive time per function from a CPU profile (microseconds)."""
    nodes = {n["id"]: n for n in profile["nodes"]}
    parent = {}
    for n in profile["nodes"]:
        for c in n.get("children", []):
            parent[c] = n["id"]

    def label(n):
        f = n["callFrame"]
        name = clean(f.get("functionName"))
        url = f.get("url", "")
        if url.endswith(".wasm") or "wasm-function" in url:
            return name
        if url:
            return f"{name} ({url.rsplit('/', 1)[-1]})"
        return name

    own = collections.Counter()
    inside = collections.Counter()
    total = 0
    for node_id, delta in zip(profile["samples"], profile["timeDeltas"]):
        delta = max(delta, 0)
        total += delta
        own[label(nodes[node_id])] += delta
        seen = set()
        n = node_id
        while n is not None:
            lab = label(nodes[n])
            if lab not in seen:
                seen.add(lab)
                inside[lab] += delta
            n = parent.get(n)
    skip = {"(root)", "(program)", "(idle)", "(garbage collector)"}
    idle = own.get("(idle)", 0)

    def rows(counter):
        return [{"function": k, "ms": round(v / 1000, 1), "pct": round(100 * v / max(total, 1), 1)}
                for k, v in counter.most_common() if k not in skip][:top]

    return {
        "sampled_s": round(total / 1e6, 2),
        "busy_pct": round(100 * (total - idle) / max(total, 1), 1),
        "program_pct": round(100 * own.get("(program)", 0) / max(total, 1), 1),
        "gc_pct": round(100 * own.get("(garbage collector)", 0) / max(total, 1), 1),
        "own": rows(own),
        "inside": rows(inside),
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--resources", type=pathlib.Path, required=True, help="prepared resources zip")
    ap.add_argument("--dist", type=pathlib.Path, default=ROOT / "web" / "dist")
    ap.add_argument("--seconds", type=float, default=30, help="how long to record")
    ap.add_argument("--settle", type=float, default=10, help="seconds after joining before it")
    ap.add_argument("--spikes", type=float, default=25, help="a slow frame, in ms (HORIZON_SPIKES)")
    ap.add_argument("--still", action="store_true", help="stand still instead of flying forward")
    ap.add_argument("--channel", help="a browser of this machine: chrome, msedge (default: "
                    "Playwright's Chromium on SwiftShader)")
    ap.add_argument("--seed", default="777")
    ap.add_argument("--size", default="1280x720", help="the page's size (smaller: less GPU work)")
    ap.add_argument("--render-distance", type=int, help="chunks (the game's --render-distance)")
    ap.add_argument("--top", type=int, default=25, help="functions listed")
    ap.add_argument("--join-timeout", type=float, default=300)
    ap.add_argument("--out", type=pathlib.Path, help="also write the whole report here, as JSON")
    ap.add_argument("--cpuprofile", type=pathlib.Path,
                    help="also write the page thread's CPU profile here (.cpuprofile: Chrome "
                    "DevTools' Performance panel and speedscope.app open it)")
    args = ap.parse_args()
    dist = args.dist.resolve()
    if not (dist / "pkg").is_dir():
        sys.exit(f"web_profile: no {dist}/pkg: build it first (web/build.sh)")
    from playwright.sync_api import sync_playwright

    port = free_port()
    server = subprocess.Popen(
        [sys.executable, str(ROOT / "web" / "serve.py"), "--port", str(port), "--dist", str(dist),
         "--resources", str(args.resources.resolve())],
        cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
    try:
        deadline = time.time() + 20
        while time.time() < deadline:
            try:
                socket.create_connection(("127.0.0.1", port), timeout=1).close()
                break
            except OSError:
                time.sleep(0.2)
        with sync_playwright() as pw:
            launch = {"headless": False, "args": (GPU if args.channel else SWIFTSHADER) + COMMON}
            if args.channel:
                launch["channel"] = args.channel
            browser = pw.chromium.launch(**launch)
            width, height = (int(v) for v in args.size.split("x"))
            page = browser.new_page(viewport={"width": width, "height": height})
            log, errors = [], []
            page.on("console", lambda m: log.append(m.text))
            page.on("pageerror", lambda e: errors.append(str(e)))
            page.add_init_script(RAF)
            page.goto(f"http://127.0.0.1:{port}/?env=HORIZON_SPIKES={args.spikes:g}")
            page.wait_for_selector("#play:not([disabled])", timeout=180_000)
            page.evaluate(
                "a => document.getElementById('args').value = a",
                f"--world Profile{int(time.time())} --creative --seed {args.seed}"
                + (f" --render-distance {args.render_distance}" if args.render_distance else ""),
            )
            page.click("#play")
            started = time.time()
            while not any("logged in with entity id" in line for line in log):
                if errors or time.time() - started > args.join_timeout:
                    sys.exit(f"web_profile: no join: {errors[:3]}")
                page.wait_for_timeout(500)
            print(f"web_profile: joined after {time.time() - started:.1f} s; settling "
                  f"{args.settle:g} s", flush=True)
            page.wait_for_timeout(args.settle * 1000)
            page.click("canvas")
            if not args.still:
                # Creative flight: Space twice, then forward at a sprint.
                for _ in range(2):
                    page.keyboard.press("Space")
                    page.wait_for_timeout(120)
                page.keyboard.down("Control")
                page.keyboard.down("w")
            cdp = page.context.new_cdp_session(page)
            cdp.send("Profiler.enable")
            cdp.send("Profiler.setSamplingInterval", {"interval": 250})
            page.evaluate("globalThis.horizonGlue.horizon_frame_stats_ask(true)")
            page.evaluate("globalThis.__rec(true)")
            mark = len(log)
            cdp.send("Profiler.start")
            print(f"web_profile: recording {args.seconds:g} s"
                  f"{'' if args.still else ', flying forward'}", flush=True)
            page.wait_for_timeout(args.seconds * 1000)
            profile = cdp.send("Profiler.stop")["profile"]
            if args.cpuprofile:
                args.cpuprofile.write_text(json.dumps(profile))
            stamps = page.evaluate("globalThis.__rec(false), globalThis.__rafs")
            page.evaluate("globalThis.horizonGlue.horizon_frame_stats_ask(false)")
            stats = None
            for _ in range(50):
                stats = page.evaluate("globalThis.horizonGlue.horizon_frame_stats()")
                if stats:
                    break
                page.wait_for_timeout(100)
            if not args.still:
                page.keyboard.up("w")
                page.keyboard.up("Control")
            report = {
                "build": json.loads((dist / "build.json").read_text()),
                "browser": args.channel or "chromium (SwiftShader)",
                "moving": not args.still,
                "frames": frame_report(stamps),
                "game": json.loads(stats) if stats else None,
                "slow_frames": spike_report(log[mark:]),
                "page_thread": cpu_report(profile, args.top),
                "errors": errors,
            }
            browser.close()
    finally:
        server.terminate()
    print_report(report)
    if args.out:
        args.out.write_text(json.dumps(report, indent=1))
        print(f"\nweb_profile: the whole report is in {args.out}")


def print_report(r):
    f = r["frames"]
    print(f"\n## Frames ({r['browser']}, {r['build'].get('profile')} build, "
          f"{'flying' if r['moving'] else 'still'})")
    print(f"{f['frames']} frames in {f['seconds']:.1f} s: median {f['p50_ms']:.1f} ms, "
          f"p95 {f['p95_ms']:.1f}, p99 {f['p99_ms']:.1f}, max {f['max_ms']:.1f}; "
          f"over 33 ms {f['over_33ms']}, over 50 ms {f['over_50ms']}, over 100 ms {f['over_100ms']}")
    print("slowest:", ", ".join(f"{w['ms']} ms at {w['at_s']} s" for w in f["worst"][:6]))
    g = r["game"]
    if g and g.get("frames"):
        cpu, gpu = g.get("cpu") or {}, g.get("gpu") or {}
        print(f"\n## The game's figures")
        print(f"CPU per frame: median {cpu.get('p50_ms', 0):.1f} ms, p99 {cpu.get('p99_ms', 0):.1f}, "
              f"max {cpu.get('max_ms', 0):.1f}")
        if gpu:
            print(f"GPU per frame: median {gpu.get('p50_ms', 0):.1f} ms, p99 {gpu.get('p99_ms', 0):.1f}, "
                  f"max {gpu.get('max_ms', 0):.1f}")
        print("phases, average ms:", json.dumps(g.get("phases_avg_ms")))
        if g.get("slow_phases_avg_ms") is not None:
            print("phases in the slowest 1%:", json.dumps(g.get("slow_phases_avg_ms")))
    s = r["slow_frames"]
    print(f"\n## Slow frames' steps ({s['slow_frames']} frames over the threshold)")
    for row in s["steps"][:15]:
        print(f"  {row['step']:<40} in {row['frames']:>4} frames, {row['ms']:>8.1f} ms "
              f"(cpu {row['cpu_ms']:.1f}), worst {row['max_ms']:.1f}")
    for line in s["slowest"][:5]:
        print("  >", line[:240])
    p = r["page_thread"]
    print(f"\n## The page's thread ({p['sampled_s']} s sampled, busy {p['busy_pct']}%, "
          f"browser internals {p['program_pct']}%, GC {p['gc_pct']}%)")
    print("Own time:")
    for row in p["own"]:
        print(f"  {row['pct']:>5.1f}%  {row['ms']:>8.1f} ms  {row['function'][:150]}")
    print("Time inside (with what they call):")
    for row in p["inside"]:
        print(f"  {row['pct']:>5.1f}%  {row['ms']:>8.1f} ms  {row['function'][:150]}")
    if r["errors"]:
        print("\nPage errors:", r["errors"][:5])


if __name__ == "__main__":
    main()

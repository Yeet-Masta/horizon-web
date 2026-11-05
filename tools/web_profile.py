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
  the same for single player's server thread, whose slow ticks hold back what the world does;
- the page's thread from a CPU profile: the functions it spent its time in (own time, and time
  inside them with what they call), the wasm module's by name. A call out to the browser (a
  WebGPU call, a canvas, a message) counts in the module's JavaScript glue for it
  (`__wbg_...`), so the glue's own time is the browser's work for those calls.
- the first --settle seconds in the world on their own (joining: terrain and first-use work);
- with --trace, a Chrome trace of the recording and what its busiest threads did: the GPU
  process's (WebGPU, Dawn, the driver: shader compiles show there), the page's, the compositor's;
- the machine: the browser, its WebGPU adapter, the screen.

--profile-dir keeps the browser's profile, its GPU shader cache with it, between runs: a run that
joins smoothly the second time compiled shaders the first.

    python3 tools/web_profile.py --resources .local/horizon-web-resources.zip
    python3 tools/web_profile.py --resources R --channel chrome     # the installed Chrome, its GPU
    python3 tools/web_profile.py --resources R --dist D --out profile.json
    python3 tools/web_profile.py --resources R --channel chrome --seconds 15 --trace trace.json

Without --channel it uses Playwright's Chromium on SwiftShader (no GPU): its CPU profile holds,
its GPU times don't. A build whose module lost its names (wasm-opt strips them) shows addresses.
Needs Playwright for Python (pip install playwright; python -m playwright install chromium).
"""
import argparse
import base64
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
# --trace: what Chrome's processes did, the GPU process's (WebGPU, Dawn, the driver) included.
TRACE_CATEGORIES = [
    "toplevel", "benchmark", "gpu", "gpu.dawn", "disabled-by-default-gpu.dawn", "webgpu",
    "disabled-by-default-webgpu", "viz", "cc", "blink", "v8.execute",
    "disabled-by-default-devtools.timeline", "disabled-by-default-devtools.timeline.frame",
]
# The browser, its GPU and the display as the page sees them.
ADAPTER = """async () => {
  const a = await navigator.gpu?.requestAdapter({powerPreference: "high-performance"});
  const i = a ? (a.info ?? (a.requestAdapterInfo ? await a.requestAdapterInfo() : {})) : {};
  return {user_agent: navigator.userAgent, cores: navigator.hardwareConcurrency,
          device_memory_gb: navigator.deviceMemory ?? null, device_pixel_ratio: devicePixelRatio,
          screen: `${screen.width}x${screen.height}`, webgpu: !!a, vendor: i.vendor ?? null,
          architecture: i.architecture ?? null, device: i.device ?? null,
          description: i.description ?? null};
}"""
# requestAnimationFrame's timestamps, kept while `__rec` is on.
RAF = """
globalThis.__rafs = []; let recording = false;
globalThis.__rec = (on) => { recording = on; if (on) globalThis.__rafs = []; };
const tick = (t) => { if (recording) globalThis.__rafs.push(t); requestAnimationFrame(tick); };
requestAnimationFrame(tick);
"""
SLOW = re.compile(r"slow frame ([\d.]+) ms[^(]*\([^)]*\): (.*)$")
# The server thread's (single player's integrated server): `slow tick 85.2 ms (140 mobs): ...`.
SLOW_TICK = re.compile(r"slow tick ([\d.]+) ms[^(]*\([^)]*\): (.*)$")
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
        "last_over_100ms_at_s": max((round((stamps[i + 1] - stamps[0]) / 1000, 1)
                                     for i, g in enumerate(gaps) if g > 100), default=None),
        "worst": [{"ms": round(g, 1), "at_s": round(at / 1000, 1)} for g, at in worst],
    }


def spike_report(lines, pattern=SLOW, what="slow frame"):
    """The slow-frame lines' steps (or the slow ticks'): in how many each came, and its time there."""
    steps = collections.defaultdict(lambda: {"frames": 0, "ms": 0.0, "cpu_ms": 0.0, "max_ms": 0.0})
    frames = []
    for line in lines:
        m = pattern.search(line)
        if not m:
            continue
        frames.append((float(m.group(1)), line[line.index(what):]))
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


def trace_report(events, top=10):
    """Per thread of the GPU process, the page's renderer and the compositor: how busy it was,
    the events its time went to (self time: an event's own, less the events inside it), and its
    longest tasks with the events they spent most of it in (milliseconds)."""
    procs, threads = {}, {}
    for e in events:
        if e.get("ph") == "M" and e.get("name") == "process_name":
            procs[e["pid"]] = e.get("args", {}).get("name", "")
        elif e.get("ph") == "M" and e.get("name") == "thread_name":
            threads[(e["pid"], e["tid"])] = e.get("args", {}).get("name", "")
    spans = collections.defaultdict(list)
    open_ = collections.defaultdict(list)
    first, last = None, None
    for e in events:
        ph, ts = e.get("ph"), e.get("ts")
        if ts is None or ph not in ("X", "B", "E"):
            continue
        first = ts if first is None else min(first, ts)
        last = ts if last is None else max(last, ts)
        key = (e.get("pid"), e.get("tid"))
        if ph == "X":
            spans[key].append((ts, e.get("dur", 0), e.get("name", "")))
        elif ph == "B":
            open_[key].append((ts, e.get("name", "")))
        elif open_[key]:
            start, name = open_[key].pop()
            spans[key].append((start, ts - start, name))
    if first is None:
        return {"seconds": 0, "threads": []}
    rows = []
    for key, items in spans.items():
        process = procs.get(key[0], "")
        thread = threads.get(key, str(key[1]))
        if not (process.startswith("GPU") or thread in ("CrRendererMain", "VizCompositorThread")):
            continue
        items.sort(key=lambda t: (t[0], -t[1]))
        # Nesting: [ts, end, name, time in children, children].
        nodes, outer, stack = [], [], []
        for ts, dur, name in items:
            while stack and ts >= nodes[stack[-1]][1]:
                stack.pop()
            node = [ts, ts + dur, name, 0, []]
            if stack:
                parent = nodes[stack[-1]]
                parent[3] += min(dur, parent[1] - ts)
                parent[4].append(len(nodes))
            else:
                outer.append(len(nodes))
            stack.append(len(nodes))
            nodes.append(node)
        own = collections.defaultdict(lambda: [0, 0])
        for n in nodes:
            own[n[2]][0] += 1
            own[n[2]][1] += max(n[1] - n[0] - n[3], 0)
        busy = sum(nodes[i][1] - nodes[i][0] for i in outer)

        def path(i):
            names, total = [nodes[i][2]], nodes[i][1] - nodes[i][0]
            while nodes[i][4]:
                i = max(nodes[i][4], key=lambda k: nodes[k][1] - nodes[k][0])
                if nodes[i][1] - nodes[i][0] < total / 2:
                    break
                names.append(nodes[i][2])
            return " > ".join(names[-4:])

        longest = sorted(outer, key=lambda i: nodes[i][0] - nodes[i][1])[:top]
        rows.append({
            "process": process, "thread": thread,
            "busy_pct": round(100 * busy / max(last - first, 1), 1),
            "self": [{"event": n, "count": c, "ms": round(t / 1000, 1)}
                     for n, (c, t) in sorted(own.items(), key=lambda kv: -kv[1][1])[:top]],
            "longest": [{"path": path(i), "ms": round((nodes[i][1] - nodes[i][0]) / 1000, 1),
                         "at_s": round((nodes[i][0] - first) / 1e6, 2)} for i in longest],
        })
    rows.sort(key=lambda r: -r["busy_pct"])
    return {"seconds": round((last - first) / 1e6, 1), "threads": rows[:8]}


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
    # Windows writes a redirected report in its ANSI code page: a game log line or function name
    # outside it would end the run.
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="replace")
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
    ap.add_argument("--profile-dir", type=pathlib.Path,
                    help="keep the browser's profile here between runs (its GPU shader cache: a "
                    "second run tells a first-use compile from the rest); default a fresh one")
    ap.add_argument("--trace", type=pathlib.Path,
                    help="also record a Chrome trace of the recording here (.json: DevTools' "
                    "Performance panel opens it) and sum up what the GPU process did; keep "
                    "--seconds short, the trace grows by megabytes a second")
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
            width, height = (int(v) for v in args.size.split("x"))
            viewport = {"width": width, "height": height}
            if args.profile_dir:
                args.profile_dir.mkdir(parents=True, exist_ok=True)
                context = pw.chromium.launch_persistent_context(
                    str(args.profile_dir.resolve()), viewport=viewport, **launch)
                page = context.pages[0] if context.pages else context.new_page()
            else:
                context = pw.chromium.launch(**launch).new_context(viewport=viewport)
                page = context.new_page()
            log, errors = [], []
            page.on("console", lambda m: log.append(m.text))
            page.on("pageerror", lambda e: errors.append(str(e)))
            page.add_init_script(RAF)
            page.goto(f"http://127.0.0.1:{port}/?env=HORIZON_SPIKES={args.spikes:g}")
            page.wait_for_selector("#play:not([disabled])", timeout=180_000)
            machine = page.evaluate(ADAPTER)
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
            joined = time.time() - started
            print(f"web_profile: joined after {joined:.1f} s; settling {args.settle:g} s",
                  flush=True)
            # The first seconds in the world, where terrain and first-use work pile up.
            page.evaluate("globalThis.__rec(true)")
            join_mark = len(log)
            page.wait_for_timeout(args.settle * 1000)
            join_stamps = page.evaluate("globalThis.__rafs")
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
            if args.trace:
                cdp.send("Tracing.start", {
                    "transferMode": "ReturnAsStream",
                    "traceConfig": {"recordMode": "recordAsMuchAsPossible",
                                    "includedCategories": TRACE_CATEGORIES},
                })
            cdp.send("Profiler.start")
            print(f"web_profile: recording {args.seconds:g} s"
                  f"{'' if args.still else ', flying forward'}", flush=True)
            page.wait_for_timeout(args.seconds * 1000)
            profile = cdp.send("Profiler.stop")["profile"]
            trace = None
            if args.trace:
                done = {}
                cdp.on("Tracing.tracingComplete", lambda e: done.update(e))
                cdp.send("Tracing.end")
                while "stream" not in done:
                    page.wait_for_timeout(100)
                data = bytearray()
                while True:
                    chunk = cdp.send("IO.read", {"handle": done["stream"], "size": 1 << 22})
                    piece = chunk.get("data", "")
                    data += base64.b64decode(piece) if chunk.get("base64Encoded") else piece.encode()
                    if chunk.get("eof"):
                        break
                cdp.send("IO.close", {"handle": done["stream"]})
                args.trace.write_bytes(bytes(data))
                parsed = json.loads(data)
                trace = trace_report(parsed["traceEvents"] if isinstance(parsed, dict) else parsed)
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
                "build": json.loads((dist / "build.json").read_text(encoding="utf-8")),
                "browser": args.channel or "chromium (SwiftShader)",
                "machine": machine,
                "profile_dir": bool(args.profile_dir),
                "moving": not args.still,
                "joined_s": round(joined, 1),
                "join": {
                    "frames": frame_report(join_stamps) if len(join_stamps) > 1 else None,
                    "slow_frames": spike_report(log[join_mark:mark]),
                    "slow_ticks": spike_report(log[join_mark:mark], SLOW_TICK, "slow tick"),
                },
                "frames": frame_report(stamps),
                "game": json.loads(stats) if stats else None,
                "slow_frames": spike_report(log[mark:]),
                "slow_ticks": spike_report(log[mark:], SLOW_TICK, "slow tick"),
                "page_thread": cpu_report(profile, args.top),
                "trace": trace,
                "errors": errors,
            }
            context.close()
    finally:
        server.terminate()
    print_report(report)
    if args.out:
        args.out.write_text(json.dumps(report, indent=1))
        print(f"\nweb_profile: the whole report is in {args.out}")


def print_frames(f):
    print(f"{f['frames']} frames in {f['seconds']:.1f} s: median {f['p50_ms']:.1f} ms, "
          f"p95 {f['p95_ms']:.1f}, p99 {f['p99_ms']:.1f}, max {f['max_ms']:.1f}; "
          f"over 33 ms {f['over_33ms']}, over 50 ms {f['over_50ms']}, over 100 ms {f['over_100ms']}"
          + (f"; the last over 100 ms at {f['last_over_100ms_at_s']} s"
             if f.get("last_over_100ms_at_s") is not None else ""))
    print("slowest:", ", ".join(f"{w['ms']} ms at {w['at_s']} s" for w in f["worst"][:6]))


def print_report(r):
    m = r.get("machine") or {}
    print(f"\n## The machine: {m.get('user_agent')}")
    print(f"GPU: {m.get('vendor')} {m.get('architecture')} {m.get('description') or ''} "
          f"(WebGPU {'yes' if m.get('webgpu') else 'NO'}); {m.get('cores')} threads, "
          f"screen {m.get('screen')} at {m.get('device_pixel_ratio')}x; "
          f"{'kept' if r.get('profile_dir') else 'fresh'} browser profile")
    j = r.get("join") or {}
    print(f"\n## Joining (in the world after {r.get('joined_s')} s; the first seconds there)")
    if j.get("frames"):
        print_frames(j["frames"])
    for what, key in (("frames", "slow_frames"), ("ticks", "slow_ticks")):
        part = j.get(key)
        if part and part["slow_frames"]:
            print(f"slow {what}' steps: " + ", ".join(
                f"{row['step']} {row['ms']:.0f} ms" for row in part["steps"][:8]))
    f = r["frames"]
    print(f"\n## Frames ({r['browser']}, {r['build'].get('profile')} build, "
          f"{'flying' if r['moving'] else 'still'})")
    print_frames(f)
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
    t = r.get("slow_ticks")
    if t and t["slow_frames"]:
        print(f"\n## The server thread's slow ticks ({t['slow_frames']} ticks over the threshold)")
        for row in t["steps"][:12]:
            print(f"  {row['step']:<40} in {row['frames']:>4} ticks, {row['ms']:>8.1f} ms, "
                  f"worst {row['max_ms']:.1f}")
        for line in t["slowest"][:3]:
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
    t = r.get("trace")
    if t:
        print(f"\n## The trace ({t['seconds']} s): the busiest threads")
        for row in t["threads"]:
            print(f"{row['process']} / {row['thread']}: busy {row['busy_pct']}%")
            for ev in row["self"][:6]:
                print(f"    {ev['ms']:>8.1f} ms in {ev['count']:>6}  {ev['event'][:100]}")
            for ev in row["longest"][:4]:
                print(f"    longest {ev['ms']:>7.1f} ms at {ev['at_s']:>6.2f} s: {ev['path'][:160]}")
    if r["errors"]:
        print("\nPage errors:", r["errors"][:5])


if __name__ == "__main__":
    main()

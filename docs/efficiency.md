# Efficiency harness: startup, memory and frame time

`tools/efficiency/` measures the numbers that move first when something regresses: how long a dedicated server
takes to be ready, how much memory it and the client hold, and the client's median frame time. Three runners take
the measurements, a fourth compares them with ceilings and is what CI runs.

It is not [the player-facing benchmark](benchmark.md) (`tools/perf_bench.py`: Horizon against a vanilla client on
one machine, with repeated flights and statistics) and not [the bot load bench](benchmark.md#frame-pacing)
(`tools/bench.py`: players joining a server). This harness is the cheap, repeatable check: flat worlds, one
process at a time, a handful of seconds each.

## Build and prepare

```sh
cargo build --profile fast -p horizon-client -p horizon-client-lib --bins   # target/fast/{horizon-client,horizon-ctl}
cargo run -p horizon-prepare -- all                    # .local/horizon-client/<version>/horizon-resources.json
```

The runners read the prepared resources from `.local/` (the same output every harness uses: [parity
workflow](parity-workflow.md)). `HORIZON_EFFICIENCY_BIN` and `--bin-dir` point at other binaries (a release
build, say) and `HORIZON_EFFICIENCY_SCRATCH` moves the scratch folder. Every run keeps its world, logs, game home
and launcher home under `.local/efficiency/<prefix>.<time>.<pid>/`, so nothing touches a player's real folders
and nothing lands in `/tmp`; the folder stays after the run for its log.

## The three measurements

Each writes one JSON report (defaults under `.local/efficiency/`) and prints a line per run.

### `run_server.py`: startup and memory, this checkout's server against a vanilla jar

```sh
python3 tools/efficiency/run_server.py --only horizon --runs 3
python3 tools/efficiency/run_server.py --jar ~/mc/server.jar --vanilla-home ~/mc/vanilla-libs --runs 3
```

Every run gets a fresh flat world, a free port and the same `server.properties` (offline, view distance 10); it is
timed to its listening port and to its `Done (N.NNNs)!` line, its RSS and thread count are sampled while it
starts, and it is stopped through its console (`stop`), as an operator would. The report keeps every run and the
medians per implementation.

Vanilla is your own 26.3 `server.jar`: nothing is downloaded or committed. `--vanilla-home` is a folder an
`--initSettings` run left behind (`libraries/`, `versions/`), linked into each run so the comparison measures the
server and not a library unpack.

### `run_client.py`: startup, memory and frame time, at 0 or more native jars

```sh
python3 tools/efficiency/run_client.py --counts 0,3,20 --seconds 6 --max-fps 260 --warmup 5
```

Each run launches the game offscreen through `horizon-ctl launch --wait` and measures that whole call, resource
loading included, then samples the process's RSS, benches `--seconds` of frames (`horizon-ctl bench`: `avg_fps`,
`p50_ms`, `p95_ms`, the 1% and 0.1% lows), asks `status` for the game's own memory figure and quits it. `--counts`
is a list: each entry loads the first N jars of `--mods-dir`, by file name, so the same set is measured every
time. `--append` adds to an earlier report instead of replacing it.

### `run_startup.py`: startup scaling with native jars

```sh
python3 tools/efficiency/run_startup.py --counts 0,25,50,74 --mods-dir ~/jars/native --runs 2
```

The dedicated server's time to ready and RSS as it loads 0, N and M natively runnable jars (`--java-mod` each), one
fresh flat world per run. `--mods-dir` is any folder of jars that run natively; [native mods](native-mods.md) says
which do.

## The smoke: ceilings CI enforces

```sh
python3 tools/efficiency/smoke.py                       # 3 server runs and one client run
python3 tools/efficiency/smoke.py --only server --runs 5 --out .local/efficiency/smoke.json
python3 tools/efficiency/smoke.py --thresholds my-runner.json
```

It measures the dedicated server's startup (median over `--runs`) and peak RSS, and the client's startup, peak RSS
and median frame time (offscreen, render distance 8, `maxFps 260`, `--seconds` of frames after `--warmup`), and
compares each with its ceiling. [thresholds.json](../tools/efficiency/thresholds.json) holds the defaults:

| Threshold | Default | Measures |
|---|---|---|
| `server_startup_s` | 20 s | dedicated server, wall clock until `Done (N.NNNs)!` |
| `server_rss_mib` | 1024 MiB | dedicated server, peak resident set while it starts |
| `client_startup_s` | 45 s | `horizon-ctl launch --wait` until the world is ready |
| `client_rss_mib` | 3072 MiB | client, peak resident set over the run |
| `client_p50_ms` | 33.4 ms | `horizon-ctl bench`, median frame time (two vanilla 60 FPS frames) |

They are ceilings, not targets: several times the numbers measured on the machine they were written on (2026-11-06,
16 CPUs, a loaded desktop: server 3.5 s and 250 MiB, client 4.3 s, 1.3 GiB and 0.6 ms p50 at `maxFps 260`). A
figure a few times worse than that is a regression, not noise; a machine with other hardware or a software
rasterizer keeps its own file and passes it with `--thresholds` (or `HORIZON_EFFICIENCY_THRESHOLDS`). A metric the
run could not measure (the client crashed, the server never started) fails rather than passes.

Without a prepared install `smoke.py` prints a notice and exits 0, so a machine that has no game files skips
instead of failing; `--require-install` turns that into a failure. The JSON report (`--out`) keeps every run, the
figures, the ceilings and the checks, and is appended to `$GITHUB_STEP_SUMMARY` when CI sets one.

## In CI

[`.github/workflows/perf-smoke.yml`](../.github/workflows/perf-smoke.yml) runs the self-test always, then — on a
runner with a prepared install — builds the fast profile, measures and compares, and uploads the report. It runs
weekly, by hand (`thresholds`, `runs` and `seconds` inputs), on a pull request that changes this harness, and on a
push to a `perf-smoke/*` branch. On GitHub's runners, which hold no game files, the install check finds nothing and
the job says so and skips.

```sh
python3 tools/efficiency/selftest.py     # parsing, aggregation and the thresholds, no game files needed
```

## Reading a report

`wall_to_done_s` is wall clock; `declared_done_s` is what the server's own line said, so the gap between them is
the process's start-up before the server's clock begins (dynamic linking, resource loading). `rss_peak_mib` is the
highest sample, taken every 20 ms while the server starts or at the few points around the client's bench; `rss_mib`
is the reading at ready (server) or at the end (client). `threads` is a hint about pool changes, not a threshold.
Memory comes from `/proc` on Linux and from `ps` on macOS; elsewhere there is no figure, and the smoke then fails
its memory ceilings rather than passing them.

## The nightly Java-mod corpus

Not an efficiency figure, but the other scheduled job that needs what a machine already holds rather than what it
can download: [`.github/workflows/nightly-java-corpus.yml`](../.github/workflows/nightly-java-corpus.yml) runs
`tools/real_mod_corpus.py` (one role-agnostic verdict per jar) and `tools/java_mod_server_ledger.py` (one fresh
dedicated server per jar, verdict from its log) over a real jar set. Those are the numbers
[the Java port status](java-mod-compat-status.md) reports.

```sh
gh workflow run nightly-java-corpus
gh workflow run nightly-java-corpus -f probe_limit=200 -f ledger_limit=40 -f timeout=90
gh workflow run nightly-java-corpus -f cache_restore_key=2026-11-07   # a jar set staged in the Actions cache
```

It runs nightly at 02:43 UTC and by hand. The jar set is one of these, first with a jar in it: the `corpus`
input, `.local/java-corpus` (restored from the Actions cache under `cache_restore_key`), the
`HORIZON_JAVA_CORPUS` repository variable, then `~/horizon-corpus/corpus-xl`. `neoforge` adds a second folder of
NeoForge jars and `server` an installed NeoForge server, whose libraries let the probe ask the real FML which mods
a jar declares.

As with `smoke.py`, a machine that has neither a jar set nor a prepared install is a skip rather than a failure:
the job prints a notice and runs only the harness self-tests (the synthetic corpus and the fixture metadata), so
GitHub's runners still check that the harness works. Nothing downloads a mod, a game jar or an asset, and no pull
request job ever sees a third-party jar — the fixture workflow (`real-mod-corpus.yml`) stays what a pull request
runs. Each run's `probe.json`, `ledger.json` and `ledger.txt` land in `.local/java-corpus-reports/` as the
`java-corpus-reports` artifact, with the counts and the top refusal causes in the job's summary.

To stage a jar set in the Actions cache, put the jars in `.local/java-corpus` on a runner that keeps its checkout
and dispatch with `cache_save_key`; a key is immutable, so a refresh takes a new one.

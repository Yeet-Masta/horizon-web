# Efficiency harness

Benchmarks for the numbers a regression moves first — a dedicated server's startup and memory, a client's
startup, memory and median frame time — and the smoke CI runs against ceilings.

- `bench_lib.py`: shared paths, scratch folders, ports, `/proc` sampling, server runs, thresholds.
- `run_server.py`: dedicated server against your own vanilla 26.3 `server.jar`.
- `run_client.py`: startup and `horizon-ctl bench` frame times, at 0 or more native jars.
- `run_startup.py`: dedicated startup scaling with N native jars.
- `smoke.py`: measures both sides and compares them with `thresholds.json`.
- `selftest.py`: parsing, aggregation and thresholds, with no game files needed.

Full invocation, the ceilings and the CI job: [docs/efficiency.md](../../docs/efficiency.md).

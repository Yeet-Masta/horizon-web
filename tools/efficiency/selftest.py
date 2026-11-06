"""Checks the efficiency harness's parsing, aggregation and thresholds against independent expected values.

    python3 tools/efficiency/selftest.py

No game, no build: a minute at most, and it is what CI runs before a measured run can gate anything.
"""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import unittest

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

import bench_lib as B
from smoke import metrics_of


class Scratch(unittest.TestCase):
    """A folder under the suite's scratch, removed after the test (never /tmp)."""

    def setUp(self):
        self.dir = B.scratch("selftest")

    def tearDown(self):
        shutil.rmtree(self.dir, ignore_errors=True)


class ParseDone(Scratch):
    def test_declared_seconds(self):
        line = b'[12:00:00] [Server thread/INFO]: Done (0.976s)! For help, type "help"\n'
        self.assertEqual(B.parse_done(line), 0.976)

    def test_the_first_done_line_wins(self):
        text = "Done (12.5s)!\n[Server thread/INFO]: Done (99s)!\n"
        self.assertEqual(B.parse_done(text), 12.5)

    def test_nothing_before_the_line(self):
        self.assertIsNone(B.parse_done("[Server thread/INFO]: Starting minecraft server version 26.3\n"))


class Medians(unittest.TestCase):
    def test_odd_count(self):
        self.assertEqual(B.median([3.0, 1.0, 2.0]), 2.0)

    def test_even_count_averages_the_middle_two(self):
        self.assertEqual(B.median([4.0, 1.0]), 2.5)

    def test_missing_samples_are_left_out(self):
        self.assertEqual(B.median([None, 2.0, None]), 2.0)

    def test_no_samples(self):
        self.assertIsNone(B.median([None, None]))


class Thresholds(Scratch):
    def test_a_value_at_the_ceiling_passes(self):
        rows = B.check_thresholds({"server_startup_s": 20.0}, {"server_startup_s": 20.0})
        self.assertEqual([row["ok"] for row in rows], [True])

    def test_above_the_ceiling_fails(self):
        rows = B.check_thresholds({"server_startup_s": 20.01}, {"server_startup_s": 20.0})
        self.assertFalse(rows[0]["ok"])

    def test_a_missing_figure_fails_rather_than_passes(self):
        rows = B.check_thresholds({}, {"client_p50_ms": 33.4})
        self.assertIsNone(rows[0]["value"])
        self.assertFalse(rows[0]["ok"])

    def test_only_the_ceilings_in_the_file_are_checked(self):
        rows = B.check_thresholds({"server_startup_s": 3.0, "client_p50_ms": 1.0},
                                  {"server_startup_s": 20.0})
        self.assertEqual([row["metric"] for row in rows], ["server_startup_s"])

    def test_a_file_loads_and_a_bad_key_or_value_is_refused(self):
        good = self.dir / "good.json"
        good.write_text(json.dumps({"server_rss_mib": 1024}), encoding="utf-8")
        self.assertEqual(B.load_thresholds(good), {"server_rss_mib": 1024})
        for name, body in (("unknown.json", {"frames_per_second": 60}),
                           ("negative.json", {"server_rss_mib": -1})):
            path = self.dir / name
            path.write_text(json.dumps(body), encoding="utf-8")
            with self.assertRaises(SystemExit):
                B.load_thresholds(path)


class ProcessStats(unittest.TestCase):
    @unittest.skipUnless(Path("/proc/self/status").exists(), "no /proc: the ps fallback has no thread count")
    def test_a_live_process_reports_memory_and_threads(self):
        proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(5)"])
        try:
            stats = B.proc_stats(proc.pid)
        finally:
            proc.kill()
            proc.wait()
        self.assertGreater(stats["rss_mib"], 0.0)
        self.assertGreaterEqual(stats["threads"], 1)

    def test_a_process_that_is_gone(self):
        self.assertIsNone(B.proc_stats(None))


class Prepared(Scratch):
    def test_the_marker_is_what_makes_an_install(self):
        self.assertFalse(B.prepared(self.dir))
        (self.dir / B.RESOURCES_MARKER).write_text("{}\n", encoding="utf-8")
        self.assertTrue(B.prepared(self.dir))


class NativeMods(Scratch):
    def test_the_first_count_by_name(self):
        for name in ("c.jar", "a.jar", "b.jar", "notes.txt"):
            (self.dir / name).write_text("", encoding="utf-8")
        picked = [Path(jar).name for jar in B.native_mods(2, self.dir)]
        self.assertEqual(picked, ["a.jar", "b.jar"])

    def test_zero_needs_no_folder(self):
        self.assertEqual(B.native_mods(0, None), [])

    def test_asking_for_more_than_the_folder_holds(self):
        (self.dir / "a.jar").write_text("", encoding="utf-8")
        with self.assertRaises(SystemExit):
            B.native_mods(2, self.dir)


class SmokeFigures(unittest.TestCase):
    def test_medians_over_servers_and_the_peak_rss(self):
        servers = [
            {"wall_to_done_s": 6.0, "rss_peak_mib": 300.0},
            {"wall_to_done_s": 2.0, "rss_peak_mib": 100.0},
            {"wall_to_done_s": 4.0, "rss_peak_mib": 200.0},
        ]
        client = {"startup_s": 5.0, "rss_peak_mib": 1200.0, "bench": {"p50_ms": 0.75}}
        figures = metrics_of(servers, client)
        self.assertEqual(figures, {"server_startup_s": 4.0, "server_rss_mib": 300.0,
                                   "client_startup_s": 5.0, "client_rss_mib": 1200.0,
                                   "client_p50_ms": 0.75})

    def test_a_failed_run_does_not_count(self):
        servers = [{"wall_to_done_s": 3.0, "rss_peak_mib": 250.0},
                   {"rss_peak_mib": None, "error": "the server exited with 1"}]
        client = {"error": "launch failed"}
        figures = metrics_of(servers, client)
        self.assertEqual(figures["server_startup_s"], 3.0)
        self.assertEqual(figures["server_rss_mib"], 250.0)
        self.assertIsNone(figures["client_startup_s"])
        self.assertIsNone(figures["client_p50_ms"])


if __name__ == "__main__":
    unittest.main(verbosity=2)

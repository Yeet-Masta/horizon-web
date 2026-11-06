"""Unit coverage for the native-vs-JVM differential fuzz harness.

Generation, parsing, classification and scoring are pure Python and need no
JDK, cargo or game files.  The two guarded tests compile the generated sources
when javac is present, and the end-to-end test only runs when a prebuilt
``horizon-native-java`` and ``HORIZON_FUZZ_E2E=1`` are supplied.
"""

import json
import os
import random
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

# The tool imports `lab` by putting its own folder on sys.path; restore the
# path afterwards so unittest discovery keeps resolving tests from `tests/`.
_before = list(sys.path)
from tools import java_differential_fuzz as fuzz  # noqa: E402
sys.path[:] = _before

ROOT = Path(__file__).parents[1]
SCRIPT = ROOT / "tools" / "java_differential_fuzz.py"


class FixtureGenerationTests(unittest.TestCase):
    def test_same_seed_produces_the_same_fixture(self):
        self.assertEqual(fuzz.generate_fixture(7), fuzz.generate_fixture(7))
        first, first_probes = fuzz.fixture_for(7, 0)
        second, second_probes = fuzz.fixture_for(7, 0)
        self.assertEqual(first, second)
        self.assertEqual(fuzz.render_fixture(first_probes), fuzz.render_fixture(second_probes))

    def test_a_new_seed_changes_the_recipe(self):
        self.assertNotEqual(
            fuzz.generate_fixture(7).probes,
            fuzz.generate_fixture(8).probes,
        )
        self.assertNotEqual(
            fuzz.render_fixture(fuzz.fixture_for(7, 0)[1]),
            fuzz.render_fixture(fuzz.fixture_for(7, 1)[1]),
        )

    def test_one_probe_per_fixture_and_a_bounded_mutation_budget(self):
        fixture = fuzz.generate_fixture(fuzz.DEFAULT_SEED, mutations=99)
        self.assertEqual(len(fixture.probes), 1)
        self.assertLessEqual(len(fixture.mutations), fuzz.MAX_MUTATIONS)

    def test_mutations_are_reproducible_and_can_reach_a_candidate(self):
        first = fuzz.generate_fixture(21, mutations=6)
        second = fuzz.generate_fixture(21, mutations=6)
        self.assertEqual(first.mutations, second.mutations)
        self.assertEqual(first.parameter_seed, second.parameter_seed)
        reached = any(
            any(
                name in mutation
                for name in ("probe_string_format", "probe_stream", "probe_uuid")
            )
            for seed in range(64)
            for mutation in fuzz.generate_fixture(seed, mutations=6).mutations
        )
        self.assertTrue(reached, "mutations never reached a candidate surface")

    def test_rendered_source_is_a_registering_entrypoint(self):
        fixture, probes = fuzz.fixture_for(fuzz.DEFAULT_SEED, 0)
        source = fuzz.render_fixture(probes)
        self.assertTrue(source.startswith("package demo;"))
        self.assertIn("public void onInitialize()", source)
        self.assertIn("Registry.register", source)
        self.assertIn('Identifier.fromNamespaceAndPath("fuzz"', source)
        self.assertEqual(source.count("private static String p0()"), 1)
        self.assertEqual(source.count("{"), source.count("}"))

    def test_nested_types_keep_their_closing_braces(self):
        # Every probe that declares a nested type must reproduce the whole
        # block: a dropped brace is a compile error that would look like a
        # native gap in the nightly report.
        probes = []
        for name in (
            "probe_record",
            "probe_enum",
            "probe_inheritance",
            "probe_static_state",
        ):
            probes.append(fuzz.PROBE_POOL[name](random.Random(3)))
        source = fuzz.render_fixture(probes)
        for marker in ("record Pair", "enum Direction", "interface Base", "class Counter"):
            self.assertIn(marker, source)
        self.assertEqual(source.count("{"), source.count("}"))

    def test_source_is_bounded(self):
        for seed in range(20):
            _fixture, probes = fuzz.fixture_for(seed, 0)
            self.assertLess(len(fuzz.render_fixture(probes).encode("utf-8")), fuzz.MAX_SOURCE_BYTES)


class ParsingAndClassificationTests(unittest.TestCase):
    def test_native_report_reads_the_last_json_object(self):
        stdout = "\n".join([
            'noise {"status": "warning"}',
            json.dumps({"status": "ok", "registrations": [
                {"registry": "minecraft:item", "id": "fuzz:p0=1"},
            ]}),
        ])
        report = fuzz.native_report(stdout)
        self.assertEqual(report["status"], "ok")
        side = fuzz.native_side(report, 0, False, "")
        self.assertEqual(side.registrations, [("minecraft:item", "fuzz:p0=1")])

    def test_native_error_keeps_its_stage(self):
        report = fuzz.native_report(
            json.dumps({"status": "error", "stage": "execute", "error": "boom"})
        )
        side = fuzz.native_side(report, 1, False, "")
        self.assertFalse(side.ok)
        self.assertEqual(side.stage, "execute")
        self.assertEqual(side.error, "boom")

    def test_jvm_rows_ignore_other_stdout(self):
        stdout = "\n".join([
            "some warning",
            "@register\tminecraft:item\tfuzz:p0=1",
            "@register\tminecraft:block\tfuzz:p1=2",
            "trailing",
        ])
        self.assertEqual(
            fuzz.jvm_registrations(stdout),
            [("minecraft:item", "fuzz:p0=1"), ("minecraft:block", "fuzz:p1=2")],
        )

    def test_classification_matrix(self):
        jvm = fuzz.Side(True, [("minecraft:item", "fuzz:p0=1")])
        native = fuzz.Side(True, [("minecraft:item", "fuzz:p0=1")])
        self.assertEqual(fuzz.classify(native, jvm), ("ran", "match"))

        divergent = fuzz.Side(True, [("minecraft:item", "fuzz:p0=0")])
        self.assertEqual(fuzz.classify(divergent, jvm), ("refused", "host-divergence"))

        execute = fuzz.Side(False, stage="execute", error="boom")
        self.assertEqual(fuzz.classify(execute, jvm), ("refused", "execute"))

        discover = fuzz.Side(False, stage="discover", error="no entrypoint")
        self.assertEqual(fuzz.classify(discover, jvm), ("needs-loader", "discover"))

        no_report = fuzz.native_side(None, 0, False, "stderr")
        self.assertEqual(fuzz.classify(no_report, jvm), ("refused", "error"))

        timed_out = fuzz.Side(False, stage="timeout", timed_out=True)
        self.assertEqual(fuzz.classify(timed_out, jvm), ("skipped", "timeout"))

        broken_jvm = fuzz.Side(False, stage="error")
        self.assertEqual(fuzz.classify(native, broken_jvm), ("skipped", "jvm-error"))
        self.assertEqual(
            fuzz.classify(native, jvm, compile_error="javac failed"), ("skipped", "compile")
        )

    def test_diff_rows_names_the_first_mismatch(self):
        native = [("minecraft:item", "fuzz:p0=1"), ("minecraft:item", "fuzz:p1=2")]
        jvm = [("minecraft:item", "fuzz:p0=1"), ("minecraft:item", "fuzz:p1=3")]
        self.assertEqual(
            fuzz.diff_rows(native, jvm),
            {
                "missing": [["minecraft:item", "fuzz:p1=3"]],
                "extra": [["minecraft:item", "fuzz:p1=2"]],
                "first_mismatch": 1,
            },
        )


class ScoreTests(unittest.TestCase):
    PROBE = {
        "counts": {
            "total": 500, "ok": 167, "fallback": 303,
            "unsupported": 30, "metadata-only": 0,
        }
    }
    LEDGER = {
        "summary": {
            "total": 223, "ran": 87, "refused": 35, "skipped": 101,
            "reason_categories": {"none": 87},
            "recommendations": {"native": 87, "native-gap": 35, "loader": 4},
        }
    }
    FUZZ = {
        "score": {
            "fixtures": 10, "ran": 3, "refused": 7, "skipped": 0,
            "needs_loader": 0, "mutations": 27, "seed": 20261106, "match_rate": 0.3,
        }
    }

    def test_ledger_is_the_authority_for_the_native_view(self):
        score = fuzz.compatibility_score(self.PROBE, self.LEDGER, self.FUZZ, label="2026-11-06")
        native = score["native"]
        self.assertEqual(native["total"], 223)
        self.assertEqual(native["ran"], 87)
        self.assertEqual(native["refused"], 35)
        self.assertEqual(native["skipped"], 101)
        self.assertEqual(native["needs_loader"], 4)
        self.assertEqual(native["ran_rate"], 0.3901)
        self.assertTrue(score["sources"]["ledger"])

    def test_probe_verdicts_fill_in_without_a_ledger(self):
        score = fuzz.compatibility_score(self.PROBE, None, self.FUZZ, label="2026-11-06")
        native = score["native"]
        self.assertEqual(native["total"], 500)
        self.assertEqual(native["ran"], 167)
        self.assertEqual(native["refused"], 30)
        self.assertEqual(native["needs_loader"], 303)
        self.assertEqual(native["skipped"], 0)

    def test_fuzz_only_score_uses_the_fixtures(self):
        score = fuzz.compatibility_score(None, None, self.FUZZ, label="2026-11-06")
        self.assertEqual(score["overall"]["total"], 10)
        self.assertEqual(score["overall"]["ran"], 3)
        self.assertEqual(score["overall"]["ran_rate"], 0.3)

    def test_trend_compares_the_previous_run(self):
        previous = {
            "label": "2026-11-05",
            "overall": {"total": 223, "ran": 84, "refused": 37, "skipped": 102, "needs_loader": 3},
        }
        score = fuzz.compatibility_score(
            self.PROBE, self.LEDGER, self.FUZZ, previous=previous, label="2026-11-06"
        )
        self.assertEqual(
            score["trend"]["deltas"],
            {"ran": 3, "refused": -2, "skipped": -1, "needs_loader": 1},
        )

    def test_markdown_carries_counts_and_the_trend(self):
        previous = {"label": "2026-11-05", "overall": {
            "total": 223, "ran": 84, "refused": 37, "skipped": 102, "needs_loader": 3,
        }}
        text = fuzz.score_markdown(fuzz.compatibility_score(
            self.PROBE, self.LEDGER, self.FUZZ, previous=previous, label="2026-11-06"
        ))
        self.assertIn("87/223 ran (39.0%)", text)
        self.assertIn("35 refused", text)
        self.assertIn("4 needs loader", text)
        self.assertIn("compatibility trend vs 2026-11-05", text)
        self.assertIn("differential fuzz: 10 fixtures", text)

    def test_empty_inputs_still_make_a_score(self):
        score = fuzz.compatibility_score(label="2026-11-06")
        self.assertEqual(score["overall"]["total"], 0)
        self.assertEqual(score["overall"]["ran_rate"], 0.0)
        self.assertIn("0/0 ran", fuzz.score_markdown(score))


class CommandLineTests(unittest.TestCase):
    def run_script(self, *arguments):
        return subprocess.run(
            [sys.executable, str(SCRIPT), *arguments],
            text=True,
            capture_output=True,
            timeout=120,
            check=False,
        )

    def test_self_test_runs_without_a_toolchain(self):
        result = self.run_script("--self-test")
        self.assertEqual(result.returncode, 0, result.stderr or result.stdout)
        self.assertIn("self-test: ok", result.stdout)

    def test_score_only_writes_the_json_and_prints_the_summary(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name, payload in (
                ("probe.json", ScoreTests.PROBE),
                ("ledger.json", ScoreTests.LEDGER),
                ("fuzz.json", ScoreTests.FUZZ),
            ):
                (root / name).write_text(json.dumps(payload), encoding="utf-8")
            output = root / "score.json"
            result = self.run_script(
                "--score-only",
                "--probe", str(root / "probe.json"),
                "--ledger", str(root / "ledger.json"),
                "--fuzz", str(root / "fuzz.json"),
                "--label", "2026-11-06",
                "--output", str(output),
            )
            self.assertEqual(result.returncode, 0, result.stderr or result.stdout)
            self.assertIn("compatibility:", result.stdout)
            score = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(score["native"]["ran"], 87)
            self.assertEqual(score["label"], "2026-11-06")

    def test_bad_bounds_are_a_harness_error(self):
        result = self.run_script("--fixtures", "0")
        self.assertEqual(result.returncode, 1)
        self.assertIn("--fixtures", result.stderr)

    def test_a_missing_prebuilt_binary_is_a_toolchain_skip(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = self.run_script("--fixtures", "1", "--bin-dir", tmp, "--timeout", "5")
            self.assertEqual(result.returncode, 2)
            self.assertIn("skipped", result.stderr)

    @unittest.skipUnless(shutil.which("javac"), "javac is required")
    def test_generated_fixtures_compile_against_the_stubs(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            toolchain = fuzz.Toolchain(root, None, 120)
            compile_stubs = toolchain._compile_stubs("compile", fuzz.COMPILE_STUBS)
            self.assertIsNotNone(compile_stubs)
            for seed in (0, 1, 2, fuzz.DEFAULT_SEED):
                _fixture, probes = fuzz.fixture_for(seed, 0)
                source = fuzz.render_fixture(probes)
                # The class is always `demo.Entry`; give each seed its own tree.
                seed_root = root / f"seed-{seed}"
                (seed_root / "demo").mkdir(parents=True)
                source_path = seed_root / "demo" / "Entry.java"
                source_path.write_text(source, encoding="utf-8")
                classes = root / f"classes-{seed}"
                classes.mkdir()
                result = subprocess.run(
                    [toolchain.javac, "--release", "17", "-cp", str(compile_stubs),
                     "-d", str(classes), str(source_path)],
                    text=True, capture_output=True, check=False,
                )
                self.assertEqual(result.returncode, 0, f"seed {seed}: {result.stderr}")

    @unittest.skipUnless(
        shutil.which("javac") and shutil.which("java"),
        "javac and java are required",
    )
    def test_generated_fixture_registers_on_the_jvm_stub(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            toolchain = fuzz.Toolchain(root, None, 120)
            compile_stubs = toolchain._compile_stubs("compile", fuzz.COMPILE_STUBS)
            runtime_stubs = toolchain._compile_stubs("runtime", fuzz.RUNTIME_STUBS)
            self.assertIsNotNone(compile_stubs)
            self.assertIsNotNone(runtime_stubs)
            _fixture, probes = fuzz.fixture_for(fuzz.DEFAULT_SEED, 0)
            source_root = root / "src" / "demo"
            source_root.mkdir(parents=True)
            source_path = source_root / "Entry.java"
            source_path.write_text(fuzz.render_fixture(probes), encoding="utf-8")
            classes = root / "classes"
            classes.mkdir()
            compiled = subprocess.run(
                [toolchain.javac, "--release", "17", "-cp", str(compile_stubs),
                 "-d", str(classes), str(source_path)],
                text=True, capture_output=True, check=False,
            )
            self.assertEqual(compiled.returncode, 0, compiled.stderr)
            jar = root / "fixture.jar"
            fuzz.write_jar(jar, classes, "fuzz-test")
            result = subprocess.run(
                [toolchain.java, "-cp", f"{runtime_stubs}{os.pathsep}{jar}", "demo.Entry"],
                text=True, capture_output=True, check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            rows = fuzz.jvm_registrations(result.stdout)
            self.assertEqual(len(rows), 1)
            self.assertEqual(rows[0][0], "minecraft:item")
            self.assertTrue(rows[0][1].startswith("fuzz:p0="))

    @unittest.skipUnless(
        os.environ.get("HORIZON_FUZZ_E2E") == "1",
        "set HORIZON_FUZZ_E2E=1 with a prebuilt horizon-native-java to run the differential",
    )
    def test_harness_runs_both_sides_end_to_end(self):
        for candidate in (ROOT / "target/fast", ROOT / "target/debug"):
            binary = candidate / "horizon-native-java"
            if binary.is_file():
                break
        else:
            self.skipTest("no prebuilt horizon-native-java in target/fast or target/debug")
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "report.json"
            result = self.run_script(
                "--fixtures", "2", "--seed", "5", "--timeout", "60",
                "--bin-dir", str(candidate), "--json", "--output", str(output),
            )
            self.assertEqual(result.returncode, 0, result.stderr or result.stdout)
            report = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(report["schema"], 1)
            self.assertEqual(len(report["runs"]), 2)
            self.assertEqual(
                report["score"]["ran"] + report["score"]["refused"]
                + report["score"]["skipped"] + report["score"]["needs_loader"],
                2,
            )


if __name__ == "__main__":
    unittest.main()

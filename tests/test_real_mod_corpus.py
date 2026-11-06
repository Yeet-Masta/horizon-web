"""Unit coverage for the real-mod corpus harness.

Every jar here is synthetic and written to a temp directory; the probe toolchain is a fake, so these
tests need no JVM, no cargo and no Minecraft installation.
"""

import contextlib
import io
import json
import shutil
import tempfile
import unittest
import zipfile
from pathlib import Path

from tools import real_mod_corpus as corpus

DISCOVER = {"status": "error", "stage": "discover", "error": "no main entrypoint"}
OK_MAIN = {"status": "ok", "entrypoints": ["com.example.Demo"], "host_calls": 2}


def response(payload=None, *, stdout="", code=0, timed_out=False):
    text = json.dumps(payload) if payload is not None else stdout
    return corpus.Probe("", "", code, timed_out, text, "")


class FakeToolchain:
    """A ProbeToolchain stand-in: answers by (tool, mode), records every call."""

    timeout = 1.0

    def __init__(self, answers, available=True, reason=None):
        self.answers, self.available, self.reason = answers, available, reason
        self.calls, self.builds = [], 0

    def build(self):
        self.builds += 1
        return self.available, self.reason

    def probe(self, tool, mode, args):
        self.calls.append((tool, mode))
        if (tool, mode) not in self.answers:
            raise AssertionError(f"unexpected probe: {tool} {mode}")
        return self.answers[(tool, mode)]


class NeverBuiltToolchain:
    def build(self):
        raise AssertionError("build must not run when no jar is selected")

    def probe(self, tool, mode, args):
        raise AssertionError(f"unexpected probe: {tool} {mode}")


def nested_jar_bytes(metadata, extra=None):
    """A jar written in memory, used as a nested member of another jar."""
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        archive.writestr("fabric.mod.json", json.dumps(metadata))
        for name, data in (extra or {}).items():
            archive.writestr(name, data)
    return buffer.getvalue()


class MetadataToolchain:
    """Answers from each jar's own metadata, so an outer bundle and its nested jars differ."""

    timeout = 1.0

    def __init__(self):
        self.calls = []

    def build(self):
        return True, None

    def probe(self, tool, mode, args):
        path = Path(next(arg for arg in args if arg.endswith(".jar")))
        meta, _ = corpus.read_metadata(path, "fabric")
        kinds = meta["entrypoint_kinds"]
        self.calls.append((mode, path.name))
        if mode == "list-entrypoints":
            return response(stdout="".join(f"{kind}\tx\n" for kind in kinds))
        if mode == "mixin-plans":
            return response({"plans": [], "configs": []})
        if "main" in kinds:
            return response(OK_MAIN | {"mixin_configs": 0})
        return response(DISCOVER)


class CorpusTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.fabric, self.neoforge = self.root / "fabric", self.root / "neoforge"
        self.fabric.mkdir()
        self.neoforge.mkdir()

    def jar(self, directory, name, members):
        path = directory / name
        with zipfile.ZipFile(path, "w") as archive:
            for member, data in members.items():
                archive.writestr(member, data)
        return path

    def fabric_jar(self, name, metadata):
        return self.jar(self.fabric, name, {"fabric.mod.json": json.dumps(metadata)})

    def parse(self, *argv):
        args = corpus.parse_args(list(argv))
        args.fabric_jars = [Path(path).resolve() for path in args.fabric_jars]
        args.neoforge_jars = [Path(path).resolve() for path in args.neoforge_jars]
        return args

    def report(self, toolchain, *, neoforge=False):
        flag, root = ("--neoforge-jars", self.neoforge) if neoforge else ("--fabric-jars", self.fabric)
        return corpus.build_report(self.parse(flag, str(root)), toolchain)

    def test_fabric_main_entrypoint_is_ok(self):
        self.fabric_jar("demo.jar", {
            "schemaVersion": 1, "id": "demo", "version": "1.2.3", "name": "Demo Mod",
            "entrypoints": {"main": ["com.example.Demo"]}, "depends": {"minecraft": ">=26.3"},
        })
        toolchain = FakeToolchain({
            ("horizon-native-java", "main"): response(OK_MAIN | {"mixin_configs": 0}),
        })
        row = self.report(toolchain)["jars"][0]
        self.assertEqual(row["verdict"], "ok")
        self.assertEqual((row["loader"], row["id"], row["version"]), ("fabric", "demo", "1.2.3"))
        self.assertEqual((row["minecraft"], row["entrypoint_kinds"]), (">=26.3", ["main"]))
        self.assertIn("native main entrypoint ran", row["reason"])
        self.assertEqual(row["probes"][0]["status"], "ok")
        self.assertEqual(toolchain.calls, [("horizon-native-java", "main")])

    def test_fabric_client_only_entrypoint_is_fallback(self):
        self.fabric_jar("client.jar", {
            "schemaVersion": 1, "id": "demo-client", "version": "1.0",
            "entrypoints": {"client": ["com.example.Client"]},
        })
        toolchain = FakeToolchain({
            ("horizon-native-java", "main"): response(DISCOVER),
            ("horizon-native-java", "mixin-plans"): response({"plans": [], "configs": []}),
            ("horizon-jvm", "list-entrypoints"): response(stdout="client\tcom.example.Client\n"),
        })
        row = self.report(toolchain)["jars"][0]
        self.assertEqual(row["verdict"], "fallback")
        self.assertIn("declared kinds: client (1)", row["reason"])
        self.assertIn("needs the loader or embedded JVM", row["reason"])

    def test_fabric_mixin_only_jar_is_fallback(self):
        self.fabric_jar("mixin.jar", {
            "schemaVersion": 1, "id": "demo-mixin", "version": "1.0",
            "mixins": ["demo.mixins.json"],
        })
        toolchain = FakeToolchain({
            ("horizon-native-java", "main"): response(DISCOVER),
            ("horizon-native-java", "mixin-plans"): response({"plans": [], "configs": []}),
            ("horizon-jvm", "list-entrypoints"): response(stdout=""),
        })
        row = self.report(toolchain)["jars"][0]
        self.assertEqual(row["verdict"], "fallback")
        self.assertEqual(row["reason"], "no entrypoint; 1 mixin config(s) need the loader")
        self.assertTrue(row["mixins"])
        self.assertEqual(row["mixin_configs"], 1)

    def test_neoforge_without_server_is_metadata_only(self):
        self.jar(self.neoforge, "neo.jar", {"META-INF/neoforge.mods.toml": (
            'modLoader="javafml"\nloaderVersion="[1,)"\nlicense="MIT"\n\n'
            '[[mods]]\nmodId="demo-neo"\nversion="2.0.0"\ndisplayName="Demo Neo"\n'
        )})
        toolchain = FakeToolchain({})
        report = self.report(toolchain, neoforge=True)
        self.assertEqual(report["counts"],
                         {"ok": 0, "fallback": 0, "unsupported": 0, "metadata-only": 1, "total": 1})
        row = report["jars"][0]
        self.assertEqual((row["loader"], row["id"], row["version"]),
                         ("neoforge", "demo-neo", "2.0.0"))
        self.assertIn("NeoForge server not supplied", row["reason"])
        self.assertEqual((row["probes"], toolchain.calls), ([], []))

    def test_jar_without_metadata_is_unsupported(self):
        self.jar(self.fabric, "plain.jar", {"demo/Thing.class": b"\xca\xfe\xba\xbe"})
        toolchain = FakeToolchain({})
        row = self.report(toolchain)["jars"][0]
        self.assertEqual(row["verdict"], "unsupported")
        self.assertEqual(row["reason"], "no fabric.mod.json or neoforge.mods.toml")
        self.assertEqual((row["probes"], toolchain.calls), ([], []))

    def test_duplicate_sha256_is_one_row_and_one_duplicate(self):
        source = self.fabric_jar("copy.jar", {
            "schemaVersion": 1, "id": "demo", "version": "1.0",
            "entrypoints": {"main": ["com.example.Demo"]},
        })
        shutil.copyfile(source, self.fabric / "same.jar")
        toolchain = FakeToolchain({
            ("horizon-native-java", "main"): response(OK_MAIN | {"host_calls": 0}),
        })
        report = self.report(toolchain)
        self.assertEqual(report["counts"]["total"], 1)
        self.assertEqual([row["file"] for row in report["jars"]], ["copy.jar"])
        self.assertEqual(len(report["duplicates"]), 1)
        duplicate = report["duplicates"][0]
        self.assertEqual((duplicate["file"], duplicate["same_as"]), ("same.jar", "copy.jar"))
        self.assertEqual(duplicate["sha256"], report["jars"][0]["sha256"])
        self.assertEqual(len(duplicate["sha256"]), 64)

    def test_oversized_jar_is_skipped_without_probing(self):
        self.jar(self.fabric, "huge.jar", {"blob.bin": b"\0" * (1 << 20)})
        args = self.parse("--fabric-jars", str(self.fabric), "--size-cap-mib", "1")
        report = corpus.build_report(args, NeverBuiltToolchain())
        self.assertEqual(report["counts"],
                         {verdict: 0 for verdict in corpus.VERDICTS} | {"total": 0})
        self.assertEqual(report["jars"], [])
        self.assertIn("no jar left to probe: 1 skipped, 0 deferred", report["skipped"])
        self.assertIn("past the 1048576-byte cap", report["skipped_jars"][0]["reason"])

    def bundle_jar(self, name, nested, *, refs=None):
        members = {
            "fabric.mod.json": json.dumps({
                "schemaVersion": 1, "id": "bundle", "version": "1.0",
                "jars": refs if refs is not None else [{"file": member} for member in nested],
            }),
        }
        members.update(nested)
        return self.jar(self.fabric, name, members)

    def test_nested_main_entrypoint_folds_outer_to_ok(self):
        inner = nested_jar_bytes({
            "schemaVersion": 1, "id": "inner", "version": "2.0",
            "entrypoints": {"main": ["com.example.Inner"]},
        })
        self.bundle_jar("bundle.jar", {"META-INF/jars/inner.jar": inner})
        row = self.report(MetadataToolchain())["jars"][0]
        self.assertEqual(row["verdict"], "ok")
        self.assertEqual(row["reason"], "nested jar(s) ran: 1 nested jar(s): 1 ok; 0 skipped")
        self.assertTrue(row["jij"])
        self.assertEqual(row["nested_skipped"], [])
        self.assertEqual(len(row["nested"]), 1)
        nested = row["nested"][0]
        self.assertEqual(
            (nested["name"], nested["loader"], nested["id"], nested["version"],
             nested["verdict"], nested["depth"]),
            ("META-INF/jars/inner.jar", "fabric", "inner", "2.0", "ok", 1),
        )
        self.assertIn("native main entrypoint ran", nested["reason"])

    def test_nested_referenced_outside_well_known_dirs_is_probed(self):
        inner = nested_jar_bytes({
            "schemaVersion": 1, "id": "inner", "version": "1.0",
            "entrypoints": {"main": ["com.example.Inner"]},
        })
        self.bundle_jar("bundle.jar", {"libs/inner.jar": inner}, refs=[{"file": "libs/inner.jar"}])
        row = self.report(MetadataToolchain())["jars"][0]
        self.assertEqual((row["verdict"], row["nested"][0]["id"]), ("ok", "inner"))

    def test_nested_fallback_keeps_outer_fallback(self):
        inner = nested_jar_bytes({
            "schemaVersion": 1, "id": "inner-client", "version": "1.0",
            "entrypoints": {"client": ["com.example.Client"]},
        })
        self.bundle_jar("bundle.jar", {"META-INF/jars/inner.jar": inner})
        row = self.report(MetadataToolchain())["jars"][0]
        self.assertEqual(row["verdict"], "fallback")
        self.assertEqual(
            row["reason"],
            "nested jars probed, none ok (1 nested jar(s): 1 fallback; 0 skipped)",
        )
        self.assertEqual(row["nested"][0]["verdict"], "fallback")

    def test_nested_duplicate_is_probed_once(self):
        inner = nested_jar_bytes({
            "schemaVersion": 1, "id": "inner", "version": "1.0",
            "entrypoints": {"main": ["com.example.Inner"]},
        })
        self.bundle_jar("bundle.jar", {
            "META-INF/jars/inner-a.jar": inner,
            "META-INF/jars/inner-b.jar": inner,
        })
        toolchain = MetadataToolchain()
        row = self.report(toolchain)["jars"][0]
        self.assertEqual([nested["id"] for nested in row["nested"]], ["inner"])
        self.assertEqual(len(row["nested_skipped"]), 1)
        self.assertIn("duplicate of META-INF/jars/inner-a.jar", row["nested_skipped"][0]["reason"])
        self.assertEqual(sum(1 for mode, _ in toolchain.calls if mode == "main"), 2)

    def test_nested_limit_caps_rows(self):
        nested = {
            f"META-INF/jars/inner-{index:02d}.jar": nested_jar_bytes({
                "schemaVersion": 1, "id": f"inner-{index}", "version": "1.0",
                "entrypoints": {"main": ["com.example.Inner"]},
            })
            for index in range(5)
        }
        self.bundle_jar("bundle.jar", nested)
        args = self.parse("--fabric-jars", str(self.fabric), "--nested-limit", "3")
        row = corpus.build_report(args, MetadataToolchain())["jars"][0]
        self.assertEqual([item["id"] for item in row["nested"]],
                         ["inner-0", "inner-1", "inner-2"])
        self.assertEqual(len(row["nested_skipped"]), 1)
        self.assertIn("per-jar nested limit 3", row["nested_skipped"][0]["reason"])
        self.assertEqual(row["verdict"], "ok")

    def test_nested_depth_bounds_recursion(self):
        leaf = nested_jar_bytes({
            "schemaVersion": 1, "id": "leaf", "version": "1.0",
            "entrypoints": {"main": ["com.example.Leaf"]},
        })
        middle = nested_jar_bytes({
            "schemaVersion": 1, "id": "middle", "version": "1.0",
            "jars": [{"file": "META-INF/jars/leaf.jar"}],
        }, {"META-INF/jars/leaf.jar": leaf})
        self.bundle_jar("bundle.jar", {"META-INF/jars/middle.jar": middle})

        shallow = corpus.build_report(
            self.parse("--fabric-jars", str(self.fabric)), MetadataToolchain()
        )["jars"][0]
        self.assertEqual([item["id"] for item in shallow["nested"]], ["middle"])
        self.assertEqual(shallow["verdict"], "fallback")

        deep = corpus.build_report(
            self.parse("--fabric-jars", str(self.fabric), "--nested-depth", "2"),
            MetadataToolchain(),
        )["jars"][0]
        self.assertEqual([item["id"] for item in deep["nested"]], ["middle", "leaf"])
        self.assertEqual([item["depth"] for item in deep["nested"]], [1, 2])
        self.assertEqual(deep["verdict"], "ok")

    def test_nested_size_cap_skips_without_probing(self):
        big = nested_jar_bytes(
            {"schemaVersion": 1, "id": "big", "version": "1.0"},
            {"blob.bin": b"\0" * (1 << 20)},
        )
        self.bundle_jar("bundle.jar", {"META-INF/jars/big.jar": big})
        args = self.parse("--fabric-jars", str(self.fabric), "--nested-size-cap-mib", "1")
        row = corpus.build_report(args, MetadataToolchain())["jars"][0]
        self.assertEqual(row["nested"], [])
        self.assertEqual(len(row["nested_skipped"]), 1)
        self.assertIn("past the 1048576-byte nested cap", row["nested_skipped"][0]["reason"])
        self.assertEqual(row["verdict"], "fallback")

    def test_no_nested_keeps_old_output_shape(self):
        inner = nested_jar_bytes({
            "schemaVersion": 1, "id": "inner", "version": "1.0",
            "entrypoints": {"main": ["com.example.Inner"]},
        })
        self.bundle_jar("bundle.jar", {"META-INF/jars/inner.jar": inner})
        toolchain = MetadataToolchain()
        report = corpus.build_report(
            self.parse("--fabric-jars", str(self.fabric), "--no-nested"), toolchain
        )
        row = report["jars"][0]
        self.assertNotIn("nested", row)
        self.assertNotIn("nested_skipped", row)
        self.assertNotIn("nested", report["limits"])
        self.assertEqual(row["verdict"], "fallback")
        self.assertIn("probe the nested jars", row["reason"])
        self.assertFalse(any(name.startswith("nested-") for _, name in toolchain.calls))

    def test_nested_report_is_deterministic_json(self):
        inner = nested_jar_bytes({
            "schemaVersion": 1, "id": "inner", "version": "1.0",
            "entrypoints": {"main": ["com.example.Inner"]},
        })
        self.bundle_jar("bundle.jar", {"META-INF/jars/inner.jar": inner})
        reports = [corpus.build_report(
            self.parse("--fabric-jars", str(self.fabric)), MetadataToolchain()
        ) for _ in range(2)]
        first, second = (json.dumps(report, sort_keys=True) for report in reports)
        self.assertEqual(first, second)

    def test_cli_parsing(self):
        args = self.parse("--fabric-jars", str(self.fabric), "--limit", "2", "--json")
        self.assertEqual((args.limit, args.json, args.timeout), (2, True, 60.0))
        self.assertEqual((args.size_cap_mib, args.hash_budget_mib), (64, 1024))
        self.assertTrue(self.parse("--self-test").self_test)
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as caught:
                corpus.parse_args(["--limit", "0"])
            self.assertEqual(caught.exception.code, 1)
            with self.assertRaises(SystemExit):
                corpus.parse_args(["--fabric-jars", str(self.root / "missing")])

    def test_report_is_deterministic_json(self):
        self.fabric_jar("demo.jar", {
            "schemaVersion": 1, "id": "demo", "version": "1.0",
            "entrypoints": {"main": ["com.example.Demo"]},
        })
        self.jar(self.fabric, "plain.jar", {"demo/Thing.class": b"\xca\xfe\xba\xbe"})
        reports = []
        for _ in range(2):
            toolchain = FakeToolchain({
                ("horizon-native-java", "main"): response(OK_MAIN | {"host_calls": 3}),
            })
            reports.append(self.report(toolchain))
        first, second = (json.dumps(report, sort_keys=True) for report in reports)
        self.assertEqual(first, second)
        self.assertEqual(reports[0]["counts"],
                         {"ok": 1, "fallback": 0, "unsupported": 1, "metadata-only": 0, "total": 2})

    def test_human_output_bounds_reasons(self):
        report = {
            "counts": {"ok": 1, "fallback": 0, "unsupported": 0, "metadata-only": 0, "total": 1},
            "skipped_jars": [], "duplicates": [], "deferred": [],
            "jars": [{"verdict": "ok", "file": "demo.jar", "id": "demo", "version": "1.0",
                      "mixins": False, "jij": False, "reason": "x" * 500}],
        }
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            corpus.print_human(report)
        lines = out.getvalue().splitlines()
        self.assertEqual(lines[0], "real mod corpus: 1 jars, 1 ok, 0 fallback, 0 unsupported")
        self.assertIn("metadata-only 0, skipped 0, duplicates 0, deferred 0", lines[1])
        self.assertTrue(lines[2].endswith("x" * 237 + "..."))

    def test_self_test_runs(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(corpus.self_test(), 0)
        self.assertEqual(out.getvalue().strip(), "real mod corpus self-test: ok")


if __name__ == "__main__":
    unittest.main()

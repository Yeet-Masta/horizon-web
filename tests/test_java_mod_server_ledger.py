import importlib.util
import json
import os
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path


ROOT = Path(__file__).parents[1]
_SPEC = importlib.util.spec_from_file_location(
    "java_mod_server_ledger_tool", ROOT / "tools" / "java_mod_server_ledger.py"
)
LEDGER = importlib.util.module_from_spec(_SPEC)
sys.modules[_SPEC.name] = LEDGER
# The tool imports `lab` by putting its own folder on sys.path; restore the
# path afterwards so unittest discovery keeps resolving tests from `tests/`.
_before = list(sys.path)
_SPEC.loader.exec_module(LEDGER)
sys.path[:] = _before


class JavaModServerLedgerTests(unittest.TestCase):
    @staticmethod
    def _jar(directory, name, *, fabric=None, neoforge=None, mod_class=False,
             fabric_raw=None, corrupt=False):
        path = Path(directory) / name
        if corrupt:
            path.write_bytes(b"not a zip")
            return path
        with zipfile.ZipFile(path, "w") as jar:
            if fabric_raw is not None:
                jar.writestr("fabric.mod.json", fabric_raw)
            elif fabric is not None:
                jar.writestr("fabric.mod.json", json.dumps(fabric))
            if neoforge is not None:
                jar.writestr("META-INF/neoforge.mods.toml", neoforge)
            if mod_class:
                jar.writestr(
                    "example/ExampleMod.class",
                    b"\xca\xfe\xba\xbeLnet/neoforged/fml/common/Mod;",
                )
        return path

    @staticmethod
    def _clipped_log(name):
        # The dedicated logger truncates at 160 characters, so the
        # `(available: ...)` tail never reaches the ledger.
        return (
            f"engine=refused id={name} version=1\n"
            f"Horizon: ERROR skipping /mods/{name}.jar: /mods/{name}.jar: "
            "no native Fabric or NeoForge entrypoint for server executio...\n"
        )

    def test_parse_success_and_counts(self):
        log = (
            "Horizon: dedicated Java compatibility: Horizon: Java compatibility /mods/apple.jar: "
            "role=dedicated-server engine=native-qualified loader=fabric id=appleskin version=3.0\n"
            "Horizon: Java compatibility: mixin weave: 0 woven, 11 skipped across 1 jar(s)\n"
            "Done (1.234s)! For help, type 'help'"
        )
        row = LEDGER.parse_log(log, "apple.jar", seconds=2.5)
        self.assertEqual((row.verdict, row.engine, row.id, row.version), ("ran", "native-qualified", "appleskin", "3.0"))
        self.assertEqual((row.woven, row.skipped), (0, 11))

    def test_parse_refusal_bounds_reason(self):
        row = LEDGER.parse_log(
            "Horizon: dedicated Java compatibility: ... engine=refused loader=fabric id=cupboard version=1.2\n"
            "dedicated Java mod startup failed: " + "x" * 1000,
            "cupboard.jar", seconds=1.0, exit_code=1)
        self.assertEqual(row.verdict, "refused")
        self.assertEqual(row.engine, "refused")
        self.assertLessEqual(len(row.reason), LEDGER.MAX_REASON)

    def test_timeout_and_summary(self):
        row = LEDGER.parse_log("starting", "slow.jar", timed_out=True, seconds=45.0)
        self.assertEqual(row.verdict, "refused")
        self.assertEqual(LEDGER.summary([row, LEDGER.Row("ok.jar", verdict="ran")]), {
            "total": 2, "ran": 1, "refused": 1, "skipped": 0,
            "reason_categories": {"none": 1, "timeout": 1},
            "recommendations": {"native-ok": 1, "native-gap": 1},
        })

    def test_parse_entrypoint_refusals_are_skipped_by_subtype(self):
        client = LEDGER.parse_log(
            "engine=refused id=visuality version=1\n"
            "refused: /mods/visuality.jar: no native Fabric or NeoForge entrypoint "
            "for server execution (available: client, modmenu)",
            "visuality.jar",
        )
        library = LEDGER.parse_log(
            "engine=refused id=ferritecore version=1\n"
            "refused: /mods/ferritecore.jar: no native Fabric or NeoForge entrypoint "
            "for server execution (available: none)",
            "ferritecore.jar",
        )
        self.assertEqual((client.verdict, client.subtype), ("skipped", "client-only"))
        self.assertEqual((library.verdict, library.subtype), ("skipped", "library"))
        self.assertIn("available: none", library.reason)

    def test_refused_engine_is_not_success_when_server_reaches_done(self):
        row = LEDGER.parse_log(
            "engine=refused id=ferritecore version=1\n"
            "ERROR: skipping mod: no native Fabric or NeoForge entrypoint for server execution (available: none)\n"
            "Done (1s)",
            "ferritecore.jar",
        )
        self.assertEqual((row.verdict, row.subtype), ("skipped", "library"))

    def test_summary_reason_categories_are_deterministic(self):
        rows = [
            LEDGER.parse_log("Done (1s)", "a.jar"),
            LEDGER.parse_log("engine=refused id=b version=1\nrefused: InvalidClass(x)", "b.jar"),
            LEDGER.parse_log("engine=refused id=c version=1\nrefused: no native Fabric or NeoForge entrypoint for server execution (available: preLaunch)", "c.jar"),
        ]
        self.assertEqual(LEDGER.summary(rows)["reason_categories"], {
            "invalid-class": 1, "no-entrypoint": 1, "none": 1,
        })

    def test_recommendation_classifies_native_loader_and_gap(self):
        ran = LEDGER.parse_log(
            "engine=native-qualified id=gamefix version=1\n"
            "mixin weave: 1 woven, 3 skipped; skipped Mixin work does not run: "
            "[target-outside-jar HEAD net/minecraft/server/level/ServerPlayer#tick, "
            "target-outside-jar HEAD net/minecraft/world/level/Level#tick, "
            "unsupported-family INVOKE demo/Target#run]\nDone (1s)",
            "gamefix.jar",
        )
        client = LEDGER.Row("client.jar", verdict="skipped", subtype="client-only", engine="refused")
        refused = LEDGER.Row("broken.jar", verdict="refused", engine="refused", reason="Metadata(bad)")
        self.assertEqual(ran.recommend, "needs-loader")
        self.assertEqual(LEDGER.recommendation(client), "native-ok")
        self.assertEqual(LEDGER.recommendation(refused), "native-gap")

    def test_recommendation_prefers_loader_engine(self):
        row = LEDGER.Row("knot.jar", verdict="refused", engine="knot")
        self.assertEqual(LEDGER.recommendation(row), "needs-loader")

    def test_clipped_client_only_jar_is_skipped_from_metadata(self):
        with tempfile.TemporaryDirectory() as tmp:
            jar = self._jar(
                tmp, "visuality.jar",
                fabric={
                    "id": "visuality",
                    "version": "1",
                    "entrypoints": {"client": ["a.Client"], "modmenu": ["b.Menu"]},
                },
            )
            row = LEDGER.parse_log(self._clipped_log("visuality"), "visuality.jar", jar=jar)
        self.assertEqual((row.verdict, row.subtype), ("skipped", "client-only"))
        self.assertIn("available: client, modmenu", row.reason)

    def test_clipped_client_hook_kinds_are_client_only(self):
        with tempfile.TemporaryDirectory() as tmp:
            jar = self._jar(
                tmp, "reeses.jar",
                fabric={
                    "id": "reeses-sodium-options",
                    "version": "2",
                    "entrypoints": {
                        "controlify": ["a.Controlify"],
                        "sodium:config_api_user": ["a.Sodium"],
                    },
                },
            )
            row = LEDGER.parse_log(self._clipped_log("reeses"), "reeses.jar", jar=jar)
        self.assertEqual((row.verdict, row.subtype), ("skipped", "client-only"))

    def test_clipped_library_jar_is_skipped_from_metadata(self):
        with tempfile.TemporaryDirectory() as tmp:
            jar = self._jar(
                tmp, "ferritecore.jar",
                fabric={"id": "ferritecore", "version": "1"},
            )
            row = LEDGER.parse_log(self._clipped_log("ferritecore"), "ferritecore.jar", jar=jar)
        self.assertEqual((row.verdict, row.subtype), ("skipped", "library"))
        self.assertIn("available: none", row.reason)

    def test_server_entrypoint_that_failed_stays_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            jar = self._jar(
                tmp, "broken.jar",
                fabric={"id": "broken", "version": "1", "entrypoints": {"main": ["a.Main"]}},
            )
            row = LEDGER.parse_log(
                "engine=refused id=broken version=1\n"
                "Horizon: ERROR skipping /mods/broken.jar: /mods/broken.jar: "
                "uncaught exception java/lang/AssertionError (at a/Main.java)",
                "broken.jar", jar=jar, exit_code=1,
            )
            # Even a clipped no-entrypoint line cannot override the jar's own
            # server-side entrypoint.
            contradicted = LEDGER.parse_log(
                self._clipped_log("broken"), "broken.jar", jar=jar)
        self.assertEqual(row.verdict, "refused")
        self.assertEqual(contradicted.verdict, "refused")

    def test_neoforge_metadata_classification(self):
        with tempfile.TemporaryDirectory() as tmp:
            client = self._jar(
                tmp, "client.jar",
                neoforge='modLoader = "javafml"\n[[mods]]\nmodId = "client"\nclientSideOnly = true\n',
            )
            server = self._jar(
                tmp, "server.jar",
                neoforge='modLoader = "javafml"\n[[mods]]\nmodId = "server"\n',
                mod_class=True,
            )
            client_row = LEDGER.parse_log(self._clipped_log("client"), "client.jar", jar=client)
            server_row = LEDGER.parse_log(self._clipped_log("server"), "server.jar", jar=server)
        self.assertEqual((client_row.verdict, client_row.subtype), ("skipped", "client-only"))
        self.assertEqual(server_row.verdict, "refused")

    def test_unreadable_jar_keeps_log_fallback(self):
        with tempfile.TemporaryDirectory() as tmp:
            corrupt = self._jar(tmp, "corrupt.jar", corrupt=True)
            clipped = LEDGER.parse_log(
                self._clipped_log("corrupt"), "corrupt.jar", jar=corrupt)
            log_available = LEDGER.parse_log(
                "refused: /mods/corrupt.jar: no native Fabric or NeoForge entrypoint "
                "for server execution (available: client, modmenu)",
                "corrupt.jar", jar=corrupt,
            )
        # No `(available: ...)` in the log and no readable metadata: the old
        # refusal stands rather than guessing a skip.
        self.assertEqual(clipped.verdict, "refused")
        self.assertEqual((log_available.verdict, log_available.subtype),
                         ("skipped", "client-only"))

    def test_metadata_reader_tolerates_raw_control_characters(self):
        with tempfile.TemporaryDirectory() as tmp:
            raw = (
                '{\n  "id": "etf",\n  "version": "7",\n'
                '  "description": "line one\nline two",\n'
                '  "entrypoints": {"client": ["a.Client"], "modmenu": ["b.Menu"]}\n}'
            )
            jar = self._jar(tmp, "etf.jar", fabric_raw=raw)
            row = LEDGER.parse_log(self._clipped_log("etf"), "etf.jar", jar=jar)
        self.assertEqual((row.verdict, row.subtype), ("skipped", "client-only"))

    def test_integration_requires_prepared_resources(self):
        resources = ROOT / ".local/horizon-client/26.3"
        if os.environ.get("HORIZON_LEDGER_INTEGRATION") != "1" or not resources.is_dir():
            self.skipTest("real-server integration is opt-in and needs prepared resources")
        self.assertTrue(resources.is_dir())


if __name__ == "__main__":
    unittest.main()

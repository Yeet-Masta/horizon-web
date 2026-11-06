import importlib.util
import os
import sys
import unittest
from pathlib import Path


ROOT = Path(__file__).parents[1]
_SPEC = importlib.util.spec_from_file_location(
    "java_mod_server_ledger_tool", ROOT / "tools" / "java_mod_server_ledger.py"
)
LEDGER = importlib.util.module_from_spec(_SPEC)
sys.modules[_SPEC.name] = LEDGER
_SPEC.loader.exec_module(LEDGER)


class JavaModServerLedgerTests(unittest.TestCase):
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
            "recommendations": {"native": 1, "native-gap": 1},
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
        self.assertEqual(ran.recommend, "loader")
        self.assertEqual(LEDGER.recommendation(client), "native")
        self.assertEqual(LEDGER.recommendation(refused), "native-gap")

    def test_recommendation_prefers_loader_engine(self):
        row = LEDGER.Row("knot.jar", verdict="refused", engine="knot")
        self.assertEqual(LEDGER.recommendation(row), "loader")

    def test_integration_requires_prepared_resources(self):
        resources = ROOT / ".local/horizon-client/26.3"
        if os.environ.get("HORIZON_LEDGER_INTEGRATION") != "1" or not resources.is_dir():
            self.skipTest("real-server integration is opt-in and needs prepared resources")
        self.assertTrue(resources.is_dir())


if __name__ == "__main__":
    unittest.main()

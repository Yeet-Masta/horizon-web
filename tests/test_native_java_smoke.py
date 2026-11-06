"""End-to-end regression for compiled Fabric and NeoForge native probe jars."""

import json
import shutil
import subprocess
import sys
import unittest
from pathlib import Path


class NativeJavaSmokeTests(unittest.TestCase):
    def test_compiled_multi_entrypoint_jar_runs_without_local_artifacts(self):
        if any(shutil.which(tool) is None for tool in ("javac", "jar", "cargo")):
            self.skipTest("javac, jar, and cargo are required for the native smoke")
        script = Path(__file__).parents[1] / "tools" / "native_java_smoke.py"
        result = subprocess.run(
            [sys.executable, str(script)],
            text=True,
            capture_output=True,
            timeout=180,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr or result.stdout)
        self.assertIn('"entrypoints": ["demo.First", "demo.Second"]', result.stdout)
        self.assertIn('"entrypoints": ["demo.Neo"]', result.stdout)
        report = json.loads(result.stdout.strip().splitlines()[-1])
        self.assertGreaterEqual(report["fabric"]["host_calls"], 10)
        self.assertEqual(report["neoforge"]["payload_directions"], ["configurationToClient"])


if __name__ == "__main__":
    unittest.main()

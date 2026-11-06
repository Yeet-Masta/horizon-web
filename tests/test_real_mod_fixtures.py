"""Build and exercise the committed source-only real-mod corpus fixtures."""

from __future__ import annotations

import json
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

from tools import real_mod_corpus


ROOT = Path(__file__).resolve().parents[1]
BUILDER = ROOT / "tools" / "real_mod_fixtures" / "build.py"

# Real sponge-mixin's `@Mixin` is CLASS-retention; keep this separately compiled fixture
# focused on proving discovery through the annotation's class bytes.
CLASS_RETENTION_SOURCES = {
    "org/spongepowered/asm/mixin/Mixin.java": (
        "package org.spongepowered.asm.mixin; import java.lang.annotation.*; "
        "@Retention(RetentionPolicy.CLASS) @Target(ElementType.TYPE) "
        "public @interface Mixin { Class<?>[] value() default {}; }"
    ),
    "org/spongepowered/asm/mixin/injection/At.java": (
        "package org.spongepowered.asm.mixin.injection; import java.lang.annotation.*; "
        "@Retention(RetentionPolicy.RUNTIME) @Target(ElementType.METHOD) "
        "public @interface At { String value(); }"
    ),
    "org/spongepowered/asm/mixin/injection/Inject.java": (
        "package org.spongepowered.asm.mixin.injection; import java.lang.annotation.*; "
        "@Retention(RetentionPolicy.RUNTIME) @Target(ElementType.METHOD) "
        "public @interface Inject { String method(); At at(); }"
    ),
    "demo/cr/Target.java": "package demo.cr; public class Target { public void tick() {} }",
    "demo/cr/ClassRetentionMixin.java": (
        "package demo.cr; import org.spongepowered.asm.mixin.Mixin; "
        "import org.spongepowered.asm.mixin.injection.*; "
        "@Mixin(Target.class) public class ClassRetentionMixin { "
        "@Inject(method=\"tick\", at=@At(\"HEAD\")) private void head() {} }"
    ),
}


class RealModFixtureTest(unittest.TestCase):
    def setUp(self) -> None:
        if shutil.which("javac") is None:
            self.skipTest("javac is unavailable")
        if shutil.which("jar") is None:
            self.skipTest("jar is unavailable")
        self.temp = tempfile.TemporaryDirectory(prefix="horizon-real-fixtures-test-")
        self.addCleanup(self.temp.cleanup)
        self.output = Path(self.temp.name) / "jars"
        result = subprocess.run(
            ["python3", str(BUILDER), "--output", str(self.output)],
            cwd=ROOT,
            text=True,
            capture_output=True,
            check=True,
        )
        self.assertTrue((self.output / "manifest.json").is_file(), result.stdout + result.stderr)
        self.manifest = json.loads((self.output / "manifest.json").read_text())
        self.fabric_root = Path(self.temp.name) / "fabric"
        self.neo_root = Path(self.temp.name) / "neo"
        self.fabric_root.mkdir()
        self.neo_root.mkdir()
        shutil.copy2(self.output / "fabric-fixture.jar", self.fabric_root / "fabric-fixture.jar")
        for name in ("neoforge-fixture.jar", "forge-fixture.jar"):
            shutil.copy2(self.output / name, self.neo_root / name)

    def corpus(self, *roots: str, server: Path | None = None) -> dict:
        argv = ["--bin-dir", str(self.bin_dir), "--timeout", "30", "--json"]
        for flag in roots:
            argv.extend([flag, str(self.fabric_root if flag == "--fabric-jars" else self.neo_root)])
        if server is not None:
            argv.extend(["--server", str(server)])
        args = real_mod_corpus.parse_args(argv)
        args.fabric_jars = [self.fabric_root] if "--fabric-jars" in roots else []
        args.neoforge_jars = [self.neo_root] if "--neoforge-jars" in roots else []
        args.bin_dir = self.bin_dir
        args.server = server
        return real_mod_corpus.build_report(args)

    @property
    def bin_dir(self) -> Path:
        for name in ("fast", "debug"):
            path = ROOT / "target" / name
            if (path / "horizon-native-java").is_file():
                return path
        return ROOT / "target" / "debug"

    def require_probes(self) -> None:
        """The corpus builds both probes up front; without them a Fabric jar is only metadata-only."""
        missing = [tool for tool in ("horizon-native-java", "horizon-jvm")
                   if not (self.bin_dir / tool).is_file()]
        if missing:
            self.skipTest(f"not built in {self.bin_dir.relative_to(ROOT)}: {', '.join(missing)}")

    def class_retention_jar(self) -> Path:
        """Compile a jar whose `@Mixin` annotation has CLASS retention, as sponge-mixin's does."""
        work = Path(self.temp.name) / "class-retention"
        source, classes = work / "src", work / "classes"
        classes.mkdir(parents=True)
        paths = []
        for name, body in CLASS_RETENTION_SOURCES.items():
            path = source / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(body, encoding="utf-8")
            paths.append(path)
        subprocess.run(
            ["javac", "-source", "8", "-target", "8", "-d", str(classes), *map(str, paths)],
            check=True,
            text=True,
            capture_output=True,
        )
        (classes / "fabric.mod.json").write_text(json.dumps({
            "schemaVersion": 1,
            "id": "class_retention_fixture",
            "version": "1.0",
            "entrypoints": {},
            "mixins": ["cr.mixins.json"],
        }))
        (classes / "cr.mixins.json").write_text(json.dumps({
            "required": True,
            "package": "demo.cr",
            "compatibilityLevel": "JAVA_8",
            "mixins": ["ClassRetentionMixin"],
        }))
        jar = work / "class-retention-fixture.jar"
        subprocess.run(
            ["jar", "--create", "--file", str(jar), "-C", str(classes), "."],
            check=True,
            text=True,
            capture_output=True,
        )
        return jar

    def test_class_retention_mixin_reports_a_plan(self) -> None:
        self.require_probes()
        probe = self.bin_dir / "horizon-native-java"
        jar = self.class_retention_jar()
        result = subprocess.run(
            [str(probe), "--mixin-plans", str(jar)],
            check=True,
            text=True,
            capture_output=True,
            timeout=120,
        )
        payload = json.loads(result.stdout.splitlines()[-1])
        self.assertEqual(payload["status"], "ok")
        self.assertEqual(len(payload["plans"]), 1)
        self.assertEqual(payload["targets"], ["demo/cr/Target#tick"])
        self.assertEqual(payload["plans"][0]["callback"], "demo/cr/ClassRetentionMixin#head")
        # The corpus folds the same probe into the fallback reason, so discovery is visible
        # without a loader: no entrypoint, but one plan instead of "one config, no plans".
        shutil.copy2(jar, self.fabric_root / jar.name)
        rows = {row["file"]: row for row in self.corpus("--fabric-jars")["jars"]}
        row = rows[jar.name]
        self.assertEqual(row["verdict"], "fallback")
        self.assertEqual(row["reason"], "no entrypoint; 1 mixin plan(s) need the loader")
        self.assertEqual(row["plans"], 1)
        self.assertEqual(row["unresolved"], 0)
        self.assertEqual(row["configs"], 1)

    def test_manifest_is_deterministic_and_fabric_nested_probe_is_folded(self) -> None:
        self.require_probes()
        self.assertEqual(set(self.manifest), {"fabric-fixture.jar", "forge-fixture.jar", "neoforge-fixture.jar"})
        report = self.corpus("--fabric-jars")
        self.assertEqual(report["counts"]["total"], 1)
        row = next(item for item in report["jars"] if item["file"] == "fabric-fixture.jar")
        self.assertEqual(row["loader"], "fabric")
        self.assertEqual(row["verdict"], "ok")
        self.assertEqual(len(row["nested"]), 1)
        self.assertEqual(row["nested"][0]["name"], "META-INF/jars/nested.jar")
        self.assertGreaterEqual(row["entrypoints"], 1)

    def test_neoforge_and_legacy_forge_metadata_only_without_server(self) -> None:
        report = self.corpus("--neoforge-jars")
        self.assertEqual(report["counts"]["metadata-only"], 2)
        for row in report["jars"]:
            self.assertEqual(row["loader"], "neoforge")
            self.assertEqual(row["verdict"], "metadata-only")
            self.assertIn("NeoForge server not supplied", row["reason"])

    def test_neoforge_path_runs_when_local_server_is_available(self) -> None:
        server = Path.home() / "horizon-corpus" / "nf263"
        if not (server / "libraries").is_dir():
            self.skipTest("no local NeoForge server install")
        self.require_probes()
        report = self.corpus("--neoforge-jars", server=server)
        rows = {row["file"]: row for row in report["jars"]}
        self.assertIn(rows["neoforge-fixture.jar"]["verdict"], {"ok", "fallback", "unsupported"})
        self.assertTrue(rows["neoforge-fixture.jar"]["probes"])


if __name__ == "__main__":
    unittest.main()

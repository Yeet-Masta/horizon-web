import importlib.util
import json
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path

ROOT = Path(__file__).parents[1]
_SPEC = importlib.util.spec_from_file_location(
    "hybrid_pack_tool", ROOT / "tools" / "hybrid_pack.py"
)
HYBRID = importlib.util.module_from_spec(_SPEC)
sys.modules[_SPEC.name] = HYBRID
# The tool imports `lab` by putting its own folder on sys.path; restore the
# path afterwards so unittest discovery keeps resolving tests from `tests/`.
_before = list(sys.path)
_SPEC.loader.exec_module(HYBRID)
sys.path[:] = _before


def _jar(directory, name, *, fabric=None, neoforge=None, native=None,
         nested=None, corrupt=False):
    path = Path(directory) / name
    if corrupt:
        path.write_bytes(b"not a zip")
        return path
    with zipfile.ZipFile(path, "w") as jar:
        if fabric is not None:
            jar.writestr("fabric.mod.json", json.dumps(fabric))
        if neoforge is not None:
            jar.writestr("META-INF/neoforge.mods.toml", neoforge)
        if native is not None:
            jar.writestr("META-INF/horizon-native.json", json.dumps(native))
        for nested_name, nested_fabric in (nested or {}).items():
            buffer = __import__("io").BytesIO()
            with zipfile.ZipFile(buffer, "w") as inner:
                inner.writestr("fabric.mod.json", json.dumps(nested_fabric))
            jar.writestr(f"META-INF/jars/{nested_name}", buffer.getvalue())
    return path


def _ledger(directory, rows):
    path = Path(directory) / "ledger.txt"
    lines = []
    for name, label in rows.items():
        field = "recommend"
        lines.append(f"{name}\t{field}={label}\tid={Path(name).stem}\tverdict=ran\tskipped=0")
    lines.append("summary\ttotal=" + str(len(rows)))
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


class HybridPackTests(unittest.TestCase):
    def test_ledger_reads_legacy_labels_and_rejects_unknown_ones(self):
        with tempfile.TemporaryDirectory() as tmp:
            ledger = _ledger(tmp, {
                "native.jar": "native",
                "loader.jar": "loader",
                "gap.jar": "native-gap",
            })
            rows = HYBRID.read_ledger(ledger)
            self.assertEqual(rows["native.jar"].label, "native-ok")
            self.assertEqual(rows["loader.jar"].label, "needs-loader")
            self.assertEqual(rows["gap.jar"].label, "native-gap")
            ledger.write_text("bad.jar\trecommend=perhaps\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                HYBRID.read_ledger(ledger)

    def test_partition_routes_by_label_and_closes_loader_dependencies(self):
        with tempfile.TemporaryDirectory() as tmp:
            _jar(tmp, "app.jar", fabric={"id": "app", "version": "1", "depends": {"lib": "*"}})
            _jar(tmp, "lib.jar", fabric={"id": "lib", "version": "1"})
            _jar(tmp, "mixin.jar", fabric={"id": "mixin", "version": "1",
                                           "depends": {"lib": "*", "fabric-api": "*"}})
            _jar(tmp, "gap.jar", fabric={"id": "gap", "version": "1"})
            _jar(tmp, "unknown.jar", fabric={"id": "unknown", "version": "1"})
            rows = HYBRID.read_ledger(_ledger(tmp, {
                "app.jar": "native-ok",
                "lib.jar": "native-ok",
                "mixin.jar": "needs-loader",
                "gap.jar": "native-gap",
            }))
            part = HYBRID.partition_pack(Path(tmp), rows)
        self.assertEqual([jar.name for jar in part.native], ["app.jar", "lib.jar"])
        # The loader half pulls in the library its mod needs; the same jar also
        # stays in the native half because another native mod needs it.
        self.assertEqual([jar.name for jar in part.loader], ["lib.jar", "mixin.jar"])
        self.assertEqual([jar.name for jar in part.shared], ["lib.jar"])
        self.assertEqual([jar.name for jar in part.excluded], ["gap.jar"])
        self.assertEqual([jar.name for jar in part.unlabelled], ["unknown.jar"])

    def test_hash_matching_labels_a_pack_jar_under_another_name(self):
        with tempfile.TemporaryDirectory() as tmp:
            pack = Path(tmp) / "pack"
            corpus = Path(tmp) / "corpus"
            pack.mkdir()
            corpus.mkdir()
            original = _jar(corpus, "corpus-name.jar",
                            fabric={"id": "renamed", "version": "1"})
            (pack / "pack-name.jar").write_bytes(original.read_bytes())
            rows = HYBRID.read_ledger(_ledger(tmp, {"corpus-name.jar": "needs-loader"}))
            index = HYBRID.build_hash_index(rows, [corpus])
            matched = HYBRID.partition_pack(pack, rows, hash_index=index)
            unmatched = HYBRID.partition_pack(pack, rows)
        self.assertEqual([jar.name for jar in matched.loader], ["pack-name.jar"])
        self.assertEqual([jar.name for jar in unmatched.unlabelled], ["pack-name.jar"])

    def test_gap_to_loader_routes_the_refused_jars(self):
        with tempfile.TemporaryDirectory() as tmp:
            _jar(tmp, "native.jar", fabric={"id": "native", "version": "1"})
            _jar(tmp, "gap.jar", fabric={"id": "gap", "version": "1"})
            rows = HYBRID.read_ledger(_ledger(tmp, {
                "native.jar": "native-ok", "gap.jar": "native-gap"}))
            strict = HYBRID.partition_pack(Path(tmp), rows)
            full = HYBRID.partition_pack(Path(tmp), rows, gap_to_loader=True)
        self.assertEqual([jar.name for jar in strict.excluded], ["gap.jar"])
        self.assertEqual([jar.name for jar in full.excluded], [])
        self.assertEqual([jar.name for jar in full.loader], ["gap.jar"])

    def test_compatibility_reports_a_dependency_split_between_halves(self):
        with tempfile.TemporaryDirectory() as tmp:
            _jar(tmp, "app.jar", fabric={"id": "app", "version": "1", "depends": {"lib": "*"}})
            _jar(tmp, "lib.jar", fabric={"id": "lib", "version": "1"})
            _jar(tmp, "mixin.jar", fabric={"id": "mixin", "version": "1",
                                           "depends": {"lib": "*"}})
            rows = HYBRID.read_ledger(_ledger(tmp, {
                "app.jar": "native-ok",
                "lib.jar": "native-ok",
                "mixin.jar": "needs-loader",
            }))
            split = HYBRID.partition_pack(Path(tmp), rows, closure=False)
            closed = HYBRID.partition_pack(Path(tmp), rows)
            split_issues = HYBRID.check_compatibility(split)
            closed_issues = HYBRID.check_compatibility(closed)
        self.assertIn(("cross-partition-dependency", ("mixin.jar",)),
                      [(i.code, i.jars) for i in split_issues])
        self.assertNotIn("cross-partition-dependency",
                         [i.code for i in closed_issues])

    def test_compatibility_flags_duplicate_ids_content_and_declared_breaks(self):
        with tempfile.TemporaryDirectory() as tmp:
            _jar(tmp, "one.jar", fabric={"id": "same", "version": "1", "breaks": {"two": "*"}},
                 native={"id": "same", "registrations": [
                     {"registry": "minecraft:item", "id": "same:wand", "kind": "item"}]})
            _jar(tmp, "two.jar", fabric={"id": "two", "version": "1"},
                 native={"id": "two", "registrations": [
                     {"registry": "minecraft:item", "id": "same:wand", "kind": "item"}]})
            _jar(tmp, "dupe.jar", fabric={"id": "same", "version": "2"})
            rows = HYBRID.read_ledger(_ledger(tmp, {
                "one.jar": "native-ok", "two.jar": "native-ok", "dupe.jar": "native-ok"}))
            issues = HYBRID.check_compatibility(HYBRID.partition_pack(Path(tmp), rows))
        codes = [issue.code for issue in issues]
        self.assertIn("duplicate-mod-id", codes)
        self.assertIn("duplicate-content", codes)
        self.assertIn("declared-break", codes)
        self.assertTrue(all(issue.severity == "conflict" for issue in issues
                            if issue.code in {"duplicate-mod-id", "duplicate-content"}))
        # `breaks` carries a version range, so it warns instead of failing.
        self.assertEqual(next(i for i in issues if i.code == "declared-break").severity,
                         "warning")

    def test_native_gap_is_a_warning_not_a_conflict(self):
        with tempfile.TemporaryDirectory() as tmp:
            _jar(tmp, "ok.jar", fabric={"id": "ok", "version": "1"})
            _jar(tmp, "gap.jar", fabric={"id": "gap", "version": "1"})
            rows = HYBRID.read_ledger(_ledger(tmp, {
                "ok.jar": "native-ok", "gap.jar": "native-gap"}))
            issues = HYBRID.check_compatibility(HYBRID.partition_pack(Path(tmp), rows))
        gap = next(issue for issue in issues if issue.code == "native-gap")
        self.assertEqual(gap.severity, "warning")

    def test_declaration_reads_fabric_content_nested_ids_and_toml_dependencies(self):
        with tempfile.TemporaryDirectory() as tmp:
            fabric = _jar(
                tmp, "fabric.jar",
                fabric={"id": "outer", "version": "3", "depends": {"lib": "*", "fabric-api": "*"},
                        "breaks": {"rival": "*"}},
                native={"id": "outer", "registrations": [
                    {"registry": "minecraft:block", "id": "outer:lamp", "kind": "block"}]},
                nested={"modules.jar": {"id": "fabric-resource-loader-v0", "version": "1"}},
            )
            toml = _jar(
                tmp, "neoforge.jar",
                neoforge=(
                    'modLoader = "javafml"\n[[mods]]\nmodId = "neo"\nversion = "2"\n'
                    '[[dependencies.neo]]\nmodId = "minecraft"\ntype = "required"\n'
                    '[[dependencies.neo]]\nmodId = "optional-lib"\ntype = "optional"\n'
                    '[[dependencies.neo]]\nmodId = "mandatory-lib"\nmandatory = true\n'
                ),
            )
            outer = HYBRID.read_declaration(fabric)
            neo = HYBRID.read_declaration(toml)
        self.assertEqual((outer.mod_id, outer.version), ("outer", "3"))
        self.assertIn("lib", outer.depends)
        self.assertIn("fabric-api", outer.depends)
        self.assertIn("fabric-resource-loader-v0", outer.nested_ids)
        self.assertEqual(outer.content, (("minecraft:block", "outer:lamp", "block"),))
        self.assertEqual(outer.breaks, ("rival",))
        self.assertEqual((neo.mod_id, neo.version), ("neo", "2"))
        # The optional dependency is dropped, the two required forms are kept
        # (platform ids are filtered by the compatibility check, not the reader).
        self.assertEqual(neo.depends, ("mandatory-lib", "minecraft"))

    def test_commands_cover_each_half_and_keep_loader_environment(self):
        with tempfile.TemporaryDirectory() as tmp:
            _jar(tmp, "native.jar", fabric={"id": "native", "version": "1"})
            _jar(tmp, "mixin.jar", fabric={"id": "mixin", "version": "1"})
            rows = HYBRID.read_ledger(_ledger(tmp, {
                "native.jar": "native-ok", "mixin.jar": "needs-loader"}))
            part = HYBRID.partition_pack(Path(tmp), rows)
            native = HYBRID.native_command(
                part.native, binary=Path("client"), resources=Path("res"), work=Path("w"))
            loader = HYBRID.loader_command(
                part.loader, binary=Path("jvm"), loader=Path("loader.jar"),
                libs=Path("libs"), game=Path("game.jar"), work=Path("w"))
            env = HYBRID.loader_env(Path("loader.jar"), Path("libs"), Path("game.jar"),
                                    Path("w"), {})
            rendered = HYBRID.render_commands(
                part, native_binary=Path("client"), jvm_binary=Path("jvm"),
                resources=Path("res"), loader=Path("loader.jar"), libs=Path("libs"),
                game=Path("game.jar"), work=Path("w"))
        self.assertEqual(native.count("--java-mod"), 1)
        self.assertTrue(native[native.index("--java-mod") + 1].endswith("native.jar"))
        self.assertLess(native.index("--java-mod"), native.index("--dedicated"))
        self.assertIn("--real-loader", loader)
        self.assertEqual([str(item) for item in loader[-1:]], [str(Path(tmp, "mixin.jar"))])
        self.assertEqual(env["HORIZON_REAL_LAUNCH"], "1")
        self.assertIn("HORIZON_LAUNCH_STOP_AT_DONE=1", rendered["loader"])
        self.assertIn("--java-mod", rendered["native"])

    def test_native_and_loader_logs_count_effective_mods(self):
        native_text = (
            "Horizon: dedicated Java compatibility: /mods/one.jar: engine=native "
            "loader=fabric entrypoints=1 registrations=2 opaque=0\n"
            "Horizon: dedicated Java compatibility: /mods/two.jar: engine=refused "
            "loader=fabric entrypoints=0 registrations=0 opaque=0\n"
        )
        native = HYBRID.parse_native(native_text, Path("/nonexistent"), 2)
        loader_text = (
            "fabric-loader: Loading 3 mods (alpha,beta,fabric-api)\n"
            "mixin-audit: applied=4 skipped=0\nDone (1.2s)!\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            alpha = _jar(tmp, "alpha.jar", fabric={"id": "alpha", "version": "1"})
            beta = _jar(tmp, "beta.jar", fabric={"id": "beta", "version": "1"})
            part = HYBRID.Partition(Path(tmp))
            part.loader = [HYBRID.PackJar(alpha, None, HYBRID.read_declaration(alpha)),
                           HYBRID.PackJar(beta, None, HYBRID.read_declaration(beta))]
        loader = HYBRID.parse_loader(loader_text, part.loader, 2)
        game_text = (
            "[18:00:00] [Thread-0/INFO]: Loading 3 mods:\n"
            "\t- alpha 1.0\n\t- beta 1.0\n\t   |-- nested-module 1.0\n"
            "[18:00:01] [Server thread/INFO]: Done (0.5s)!\n"
        )
        launched = HYBRID.parse_loader(game_text, part.loader, 2)
        failed = HYBRID.parse_loader("Loading 3 mods:\n\t- alpha 1.0\n", part.loader, 2)
        self.assertEqual((native["started"], native["executing"]), (1, 1))
        self.assertEqual(loader["loaded_ids"], ("alpha", "beta"))
        self.assertIn("mixin-audit=applied=4 skipped=0", loader["note"])
        self.assertEqual(launched["loaded_ids"], ("alpha", "beta"))
        self.assertIn("Done (", launched["note"])
        # A listing without `Done (` is not a running half.
        self.assertEqual(failed["started"], 0)
        self.assertEqual(failed["loaded_ids"], ("alpha",))


if __name__ == "__main__":
    unittest.main()

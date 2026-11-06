"""Unit coverage for the Java conformance harness (tools/java_conformance.py).

What the harness decides on its own: which exception a launcher's stderr reports, a run's verdict
against HotSpot's, what a mod jar's report gets wrong, and the class files it assembles for opcodes
javac never emits. The assembled classes also run on HotSpot when a JDK 25 is installed.
"""

import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest

from tools import java_conformance as harness


class UncaughtTest(unittest.TestCase):
    def test_exception_line_with_message_and_trace(self):
        stderr = ('Exception in thread "main" java.lang.IllegalStateException: gave up: at the bottom\n'
                  "\tat demo.Main.deeper(Main.java:8)\n")
        self.assertEqual(harness.uncaught(stderr), ("java.lang.IllegalStateException", "gave up: at the bottom"))

    def test_exception_without_message(self):
        self.assertEqual(harness.uncaught('Exception in thread "main" java.lang.Error\n'), ("java.lang.Error", None))

    def test_launcher_error_reports_its_cause(self):
        stderr = ("Error: Unable to initialize main class demo.Main\n"
                  "Caused by: java.lang.VerifyError: Operand stack overflow\n")
        self.assertEqual(harness.uncaught(stderr), ("java.lang.VerifyError", "Operand stack overflow"))

    def test_other_stderr_is_no_exception(self):
        self.assertEqual(harness.uncaught("warning: something\nmore text\n"), (None, None))


class VerdictTest(unittest.TestCase):
    ORACLE = {"exit": 0, "stdout": "1\n2\n", "exception": None}

    def run_(self, **changes):
        return dict(self.ORACLE, **changes)

    def test_same_output_passes(self):
        self.assertEqual(harness.verdict(self.ORACLE, self.run_()), "pass")

    def test_different_output_fails(self):
        self.assertEqual(harness.verdict(self.ORACLE, self.run_(stdout="1\n")), "fail")

    def test_different_exception_fails(self):
        oracle = dict(self.ORACLE, exit=1, exception="java.lang.ArithmeticException")
        self.assertEqual(harness.verdict(oracle, dict(oracle, exception="java.lang.RuntimeException")), "fail")

    def test_interpreter_error_is_unsupported(self):
        self.assertEqual(harness.verdict(self.ORACLE, self.run_(exit=harness.INTERPRETER_ERROR)), "unsupported")

    def test_panic_and_timeout(self):
        self.assertEqual(harness.verdict(self.ORACLE, self.run_(exit=101)), "crash")
        self.assertEqual(harness.verdict(self.ORACLE, self.run_(exit="timeout")), "timeout")


class ModProblemsTest(unittest.TestCase):
    EXPECT = {
        "status": "ok",
        "completed": 1,
        "ordered": True,
        "exact": True,
        "registrations": [
            {"registry": "minecraft:item", "id": "m:a",
             "declaration": {"kind": "item", "properties": {"max_stack_size": "16"}}},
            {"registry": "minecraft:item", "id": "m:b"},
        ],
    }

    def report(self, *registrations, status="ok", completed=1):
        return {"status": status, "completed": completed, "registrations": list(registrations)}

    def item(self, ident, **properties):
        return {"registry": "minecraft:item", "id": ident,
                "declaration": {"kind": "item", "properties": properties}}

    def test_matching_report_has_no_problems(self):
        got = self.report(self.item("m:a", max_stack_size="16", rarity="rare"), self.item("m:b"))
        self.assertEqual(harness.mod_problems(self.EXPECT, got), [])

    def test_wrong_property_missing_entry_and_opaque_entry(self):
        got = self.report({"registry": "minecraft:item", "id": "m:a", "declaration": None})
        problems = harness.mod_problems(self.EXPECT, got)
        self.assertIn("m:a is opaque, expected a typed item", problems)
        self.assertIn("no registration minecraft:item m:b", problems)
        got = self.report(self.item("m:a", max_stack_size="64"), self.item("m:b"))
        self.assertEqual(harness.mod_problems(self.EXPECT, got), ["m:a max_stack_size='64', expected '16'"])

    def test_order_extra_rows_and_failed_status(self):
        got = self.report(self.item("m:b"), self.item("m:a", max_stack_size="16"), self.item("m:c"),
                          status="error", completed=0)
        problems = harness.mod_problems(self.EXPECT, got)
        self.assertTrue(any(p.startswith("status 'error'") for p in problems))
        self.assertTrue(any(p.startswith("completed 0") for p in problems))
        self.assertIn("1 registration(s) more than expected", problems)
        self.assertIn("registration order m:b, m:a", problems)


class AssembleTest(unittest.TestCase):
    def test_class_file_structure(self):
        name, (desc, constants, code, stack, locals_) = next(iter(harness.GENERATED.items()))
        data = harness.assemble(name, desc, constants, code, stack, locals_)
        self.assertEqual(data[:4], b"\xca\xfe\xba\xbe")
        self.assertEqual(int.from_bytes(data[6:8], "big"), 52)
        # The constant pool count covers every entry the assembler made, and each is well formed:
        # walking them ends exactly at the access flags, and this/super name pool entries.
        count = int.from_bytes(data[8:10], "big")
        at, sizes = 10, {1: None, 3: 4, 4: 4, 7: 2, 9: 4, 10: 4, 12: 4}
        for _ in range(count - 1):
            tag = data[at]
            size = sizes[tag]
            at += 3 + int.from_bytes(data[at + 1:at + 3], "big") if size is None else 1 + size
        access, this, sup = (int.from_bytes(data[at + i:at + i + 2], "big") for i in (0, 2, 4))
        self.assertEqual(access, 0x0021)
        self.assertTrue(0 < this < count and 0 < sup < count)

    @unittest.skipUnless(shutil.which("java") or os.environ.get("JAVA_HOME"), "needs a JDK")
    def test_assembled_programs_run_on_hotspot(self):
        try:
            home = harness.java_home(None)
        except SystemExit:
            self.skipTest("needs a JDK 25")
        # What each program computes, worked out from its instructions by hand.
        expected = {
            "samples/opcodes/nop/NopGeneratedExample": "12",
            "samples/opcodes/swap/SwapGeneratedExample": "2",
            "samples/opcodes/pop2/Pop2GeneratedExample": "1000",
            "samples/opcodes/dup_x2/DupX2GeneratedExample": "1260.0",
            "samples/opcodes/dup2_x1/Dup2_X1GeneratedExample": "21321",
            "samples/opcodes/dup2_x2/Dup2_X2GeneratedExample": "214321",
        }
        with tempfile.TemporaryDirectory() as tmp:
            harness.assemble_generated(pathlib.Path(tmp))
            for name, want in expected.items():
                run = subprocess.run([str(home / "bin" / "java"), "-cp", tmp, name.replace("/", ".")],
                                     capture_output=True, text=True, env=harness.java_env())
                self.assertEqual((run.returncode, run.stdout.strip()), (0, want), f"{name}: {run.stderr}")
            run = subprocess.run([str(home / "bin" / "java"), "-cp", tmp,
                                  "samples.invalidprograms.operandstackoverflow.OperandStackOverflowExample"],
                                 capture_output=True, text=True, env=harness.java_env())
            self.assertEqual(harness.uncaught(run.stderr)[0], "java.lang.VerifyError")


if __name__ == "__main__":
    unittest.main()

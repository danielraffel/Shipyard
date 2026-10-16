"""Positive controls for run_unittest_strict.py: a planted failure, a suite
ended mid-run with status 0, and a leaked alarm each make it fail."""
import pathlib
import subprocess
import sys
import tempfile
import textwrap
import unittest

RUNNER = pathlib.Path(__file__).with_name("run_unittest_strict.py")


class StrictRunner(unittest.TestCase):
    def run_suite(self, body: str) -> subprocess.CompletedProcess:
        with tempfile.TemporaryDirectory() as tmp:
            (pathlib.Path(tmp) / "test_planted.py").write_text(textwrap.dedent(body), encoding="utf-8")
            return subprocess.run([sys.executable, str(RUNNER), "-s", tmp, "-p", "test_*.py"],
                                  capture_output=True, text=True, timeout=120, check=False)

    def test_a_passing_suite_passes(self) -> None:
        result = self.run_suite("""
            import unittest
            class T(unittest.TestCase):
                def test_one(self): pass
                def test_two(self): pass
        """)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("PASS: ran 2 tests, OK", result.stdout)

    def test_a_failing_test_fails_the_run(self) -> None:
        result = self.run_suite("""
            import unittest
            class T(unittest.TestCase):
                def test_planted_failure(self): self.assertEqual(1, 2)
        """)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("FAIL: unittest exited 1", result.stdout)

    def test_a_suite_ended_mid_run_with_status_0_fails(self) -> None:
        result = self.run_suite("""
            import os, unittest
            class T(unittest.TestCase):
                def test_a(self): pass
                def test_b_ends_the_process(self): os._exit(0)
                def test_c_never_runs(self): pass
        """)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("no 'Ran N tests' line", result.stdout)

    @unittest.skipUnless(hasattr(__import__("signal"), "SIGALRM"), "no SIGALRM")
    def test_a_leaked_alarm_that_exits_0_later_fails(self) -> None:
        result = self.run_suite("""
            import os, signal, time, unittest
            class T(unittest.TestCase):
                def test_a_leaves_an_alarm(self):
                    signal.signal(signal.SIGALRM, lambda *_: os._exit(0))
                    signal.alarm(1)
                def test_b_outlives_it(self): time.sleep(3)
        """)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("no 'Ran N tests' line", result.stdout)

    def test_verdict_needs_ok_after_the_last_summary(self) -> None:
        import importlib.util
        spec = importlib.util.spec_from_file_location("strict", RUNNER)
        strict = importlib.util.module_from_spec(spec); spec.loader.exec_module(strict)
        self.assertEqual(strict.verdict(0, "...\nRan 3 tests in 0.1s\n\nOK\n")[0], True)
        self.assertEqual(strict.verdict(0, "...\nRan 3 tests in 0.1s\n\nOK (skipped=1)\n")[0], True)
        self.assertEqual(strict.verdict(0, "...\nRan 3 tests in 0.1s\n\nFAILED (errors=1)\n")[0], False)
        self.assertEqual(strict.verdict(0, "test_a ... ok\n")[0], False)


if __name__ == "__main__":
    unittest.main()

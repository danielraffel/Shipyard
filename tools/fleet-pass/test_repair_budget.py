"""Offline tests for repair_budget.py (no network)."""
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import repair_budget as rb  # noqa: E402

A = "o/r#1 " + "a" * 40 + " HOLD"
A_PUSHED = "o/r#1 " + "b" * 40 + " HOLD"
F1 = "o/r#2 " + "c" * 40 + " FAILED:macos"
F2 = "o/r#2 " + "c" * 40 + " FAILED:macos,linux"
NOW = 1_800_000_000


class Budget(unittest.TestCase):
    def setUp(self):
        self.log = os.path.join(tempfile.mkdtemp(), "attempts.log")

    def rec(self, line, n, at=NOW - 60):
        for _ in range(n):
            rb.record(self.log, line, at)

    def test_unchanged_line_held_after_max(self):
        self.rec(A, 2)
        self.assertEqual(rb.runnable([A], self.log, NOW, 3, 3600)[0], [A])
        self.rec(A, 1)
        run, held = rb.runnable([A], self.log, NOW, 3, 3600)
        self.assertEqual(run, [])
        self.assertEqual(held, [(A, 3)])

    def test_new_head_or_new_failure_set_is_runnable(self):
        self.rec(A, 5)
        self.rec(F1, 5)
        run, _ = rb.runnable([A_PUSHED, F2], self.log, NOW, 3, 3600)
        self.assertEqual(run, [A_PUSHED, F2])

    def test_window_expiry_releases(self):
        self.rec(A, 5, at=NOW - 7200)
        self.assertEqual(rb.runnable([A], self.log, NOW, 3, 3600)[0], [A])

    def test_held_entry_does_not_block_later_entries(self):
        self.rec(A, 3)
        self.assertEqual(rb.runnable([A, F1], self.log, NOW, 3, 3600)[0], [F1])

    def test_whitespace_variants_match(self):
        self.rec(A.replace(" ", "  "), 3)
        self.assertEqual(rb.runnable([A + "  "], self.log, NOW, 3, 3600)[0], [])

    def test_missing_or_garbage_log_holds_nothing(self):
        self.assertEqual(rb.runnable([A], self.log, NOW, 1, 3600)[0], [A])
        with open(self.log, "w") as f:
            f.write("garbage\nnot-a-time\tX\n")
        self.assertEqual(rb.runnable([A], self.log, NOW, 1, 3600)[0], [A])

    def test_log_is_bounded(self):
        for i in range(rb.KEEP + 50):
            rb.record(self.log, f"o/r#{i} x HOLD", NOW)
        with open(self.log) as f:
            self.assertEqual(len(f.readlines()), rb.KEEP)

    def test_cli_filter_and_record(self):
        exe = [sys.executable, os.path.join(HERE, "repair_budget.py")]
        for _ in range(3):
            subprocess.run(exe + ["record", "--log", self.log, "--line", A, "--now", str(NOW)], check=True)
        p = subprocess.run(exe + ["filter", "--log", self.log, "--now", str(NOW)],
                           input=f"{A}\n{F1}\n", capture_output=True, text=True, check=True)
        self.assertEqual(p.stdout.splitlines(), [F1])
        self.assertEqual(p.stderr.strip(), f"HELD {A} attempts=3")


if __name__ == "__main__":
    unittest.main()

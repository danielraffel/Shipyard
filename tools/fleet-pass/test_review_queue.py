"""Offline tests for review_queue.py (no network)."""
import os
import subprocess
import sys
import tempfile
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import review_queue as rq  # noqa: E402

A = "o/r#1 " + "a" * 40
B = "o/r#2 " + "b" * 40 + " EARLY"
C = "x/y#3 " + "c" * 40


def dead_pid():
    p = subprocess.Popen([sys.executable, "-c", "pass"])
    p.wait()
    return p.pid


HEAD = "d" * 40
OLD = "e" * 40
NOW = 1_800_000_000.0
AT = "2027-01-15T08:00:00Z"  # 2 h before NOW


def st(state="success", desc=None, at=AT):
    return {"context": "review-verdict", "state": state, "created_at": at,
            "description": desc if desc is not None else f"approved: the approval of {OLD[:12]} carries to {HEAD[:12]}: x"}


class GateCarried(unittest.TestCase):
    def test_carry_counts(self):
        self.assertTrue(rq.gate_carried([st()], HEAD, {OLD}, NOW))

    def test_negative_controls(self):
        self.assertFalse(rq.gate_carried([], HEAD, {OLD}, NOW))
        self.assertFalse(rq.gate_carried([st("pending")], HEAD, {OLD}, NOW))
        self.assertFalse(rq.gate_carried([st(desc="approved: o/r does not require a review before arming")], HEAD, {OLD}, NOW))
        self.assertFalse(rq.gate_carried([st()], HEAD, set(), NOW))
        self.assertFalse(rq.gate_carried([st()], HEAD, {"f" * 40}, NOW))
        self.assertFalse(rq.gate_carried([st()], "c" * 40, {OLD}, NOW))
        self.assertFalse(rq.gate_carried([st(at="2027-01-13T08:00:00Z")], HEAD, {OLD}, NOW))
        self.assertFalse(rq.gate_carried([dict(st(), context="post-review-verdict")], HEAD, {OLD}, NOW))

    def test_newest_status_decides(self):
        older = st(at="2027-01-15T07:00:00Z")
        newer_fail = st("failure", desc="HOLD", at="2027-01-15T09:00:00Z")
        self.assertFalse(rq.gate_carried([older, newer_fail], HEAD, {OLD}, NOW))
        self.assertTrue(rq.gate_carried([newer_fail | {"created_at": "2027-01-15T06:00:00Z"}, older], HEAD, {OLD}, NOW))


class Claim(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.me = os.getpid()
        # A live pid that is not ours: our parent.
        self.other = os.getppid()

    def test_two_live_slots_never_share_an_entry(self):
        first = rq.claim([A, B, C], self.dir, self.me, cap=2)
        second = rq.claim([A, B, C], self.dir, self.other, cap=2)
        self.assertEqual(first, [A, B])
        self.assertEqual(second, [C])
        self.assertFalse(set(first) & set(second))

    def test_cap_bounds_the_slot_list(self):
        self.assertEqual(rq.claim([A, B, C], self.dir, self.me, cap=1), [A])

    def test_dead_slot_claim_is_taken_over(self):
        rq.claim([A], self.dir, dead_pid(), cap=1)
        self.assertEqual(rq.claim([A], self.dir, self.me, cap=1), [A])

    def test_live_slot_claim_is_respected(self):
        rq.claim([A], self.dir, self.other, cap=1)
        self.assertEqual(rq.claim([A, B], self.dir, self.me, cap=2), [B])

    def test_same_pid_reclaims_its_own_entry(self):
        rq.claim([A], self.dir, self.me, cap=1)
        self.assertEqual(rq.claim([A], self.dir, self.me, cap=1), [A])

    def test_pidless_claim_is_live_briefly_then_taken_over(self):
        d = os.path.join(self.dir, "o_r_1")
        os.mkdir(d)
        with open(os.path.join(d, "note"), "w") as f:  # non-empty, no pid: an older runner mid-write
            f.write("x")
        self.assertEqual(rq.claim([A], self.dir, self.me, cap=1), [])
        old = time.time() - rq.PIDLESS_GRACE - 5
        os.utime(d, (old, old))
        self.assertEqual(rq.claim([A], self.dir, self.me, cap=1), [A])

    def test_claim_is_never_visible_without_a_pid(self):
        # rename(2) publishes the directory with its pid already inside.
        rq.claim([A], self.dir, self.me, cap=1)
        with open(os.path.join(self.dir, "o_r_1", "pid")) as f:
            self.assertEqual(f.read().strip(), str(self.me))

    def test_no_temp_dirs_left_behind(self):
        rq.claim([A, B], self.dir, self.me, cap=2)
        rq.claim([A, B], self.dir, self.other, cap=2)
        self.assertEqual(sorted(os.listdir(self.dir)), ["o_r_1", "o_r_2"])

    def test_blank_lines_ignored_and_cli_prints_only_claims(self):
        out = subprocess.run([sys.executable, os.path.join(HERE, "review_queue.py"), "claim",
                              "--dir", self.dir, "--pid", str(self.me), "--cap", "5"],
                             input=f"{A}\n\n{B}\n", capture_output=True, text=True, check=True).stdout
        self.assertEqual(out.splitlines(), [A, B])


if __name__ == "__main__":
    unittest.main()

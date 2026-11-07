"""Offline tests for must-review.py: no network, `api` is replaced where needed."""
import datetime
import importlib.util
import json
import os
import tempfile
import unittest
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
_signers = tempfile.NamedTemporaryFile("w", suffix=".json", delete=False)
json.dump({"default": ["infra-review2", "drain-operator"]}, _signers)
_signers.close()
os.environ["FLEET_PASS_REVIEWER_SIGNERS"] = _signers.name
_spec = importlib.util.spec_from_file_location("mr", os.path.join(HERE, "must-review.py"))
mr = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(mr)

SHA = "a" * 40
OLD = "b" * 40
NOW = datetime.datetime(2026, 11, 7, 12, 0, tzinfo=datetime.timezone.utc)


def body(verdict="APPROVE", signer="infra-review2", session="Daniels-Mac-Studio-m3/fleet-pass-reviewer-1", sha=SHA):
    stamp = f"<!-- session: {session} -->\n" if session else ""
    return f"reviewer: {signer}\n{stamp}verdict: {verdict} head:{sha}\n"


def comment(b, at="2026-11-07T10:00:00Z"):
    return {"body": b, "created_at": at}


class Counted(unittest.TestCase):
    def test_signers_loaded_from_injected_file(self):
        self.assertEqual(mr.SIGNERS, {"infra-review2", "drain-operator"})

    def test_approve_from_reviewer_session_counts(self):
        self.assertTrue(mr.counted(body()))

    def test_approve_with_uuid_session_does_not_count(self):
        # Pulp #11704: allowlisted signer, stamp session is a uuid, not fleet-pass-reviewer-*.
        b = body(session="Daniels-Mac-Studio-m3/3b785d44-1111-2222-3333-444455556666")
        self.assertFalse(mr.counted(b))

    def test_hold_from_any_attributed_session_counts(self):
        for sess in ("Daniels-Mac-Studio-m3/3b785d44-aaaa", "host/fleet-pass-reviewer-9", "host/other"):
            self.assertTrue(mr.counted(body("HOLD", session=sess)), sess)

    def test_unattributed_stamp_never_counts(self):
        for v in ("APPROVE", "HOLD"):
            self.assertFalse(mr.counted(body(v, session="host/unattributed")))

    def test_missing_stamp_never_counts(self):
        self.assertFalse(mr.counted(body("HOLD", session=None)))

    def test_non_allowlisted_signer_never_counts(self):
        for v in ("APPROVE", "HOLD"):
            self.assertFalse(mr.counted(body(v, signer="stranger")))


class VerdictAt(unittest.TestCase):
    def test_returns_only_counted_verdicts_at_exact_head(self):
        cs = [
            comment(body(sha=OLD), "2026-11-07T09:00:00Z"),
            comment(body(signer="stranger"), "2026-11-07T09:30:00Z"),
            comment(body(session="h/3b785d44-x"), "2026-11-07T09:40:00Z"),
        ]
        self.assertEqual(mr.verdict_at(cs, SHA), (None, None))
        cs.append(comment(body("HOLD", session="h/3b785d44-x"), "2026-11-07T10:00:00Z"))
        self.assertEqual(mr.verdict_at(cs, SHA), ("HOLD", "2026-11-07T10:00:00Z"))
        cs.append(comment(body(), "2026-11-07T11:00:00Z"))
        self.assertEqual(mr.verdict_at(cs, SHA), ("APPROVE", "2026-11-07T11:00:00Z"))
        self.assertEqual(mr.verdict_at(cs, "c" * 40), (None, None))


class CheckStates(unittest.TestCase):
    def test_orders_by_id_so_stale_later_timestamp_failure_loses(self):
        runs = {"check_runs": [
            {"id": 20, "name": "macos", "status": "completed", "conclusion": "success",
             "completed_at": "2026-11-07T10:00:00Z"},
            {"id": 10, "name": "macos", "status": "completed", "conclusion": "failure",
             "completed_at": "2026-11-07T11:00:00Z"},
            {"id": 30, "name": "lint", "status": "in_progress"},
        ]}
        with mock.patch.object(mr, "api", side_effect=[runs, {"statuses": []}]):
            st = mr.check_states("o/r", SHA)
        self.assertEqual(st["macos"], "success")
        self.assertEqual(st["lint"], "pending")

    def test_statuses_ordered_by_id(self):
        statuses = {"statuses": [{"id": 2, "context": "ci", "state": "success"},
                                 {"id": 1, "context": "ci", "state": "failure"}]}
        with mock.patch.object(mr, "api", side_effect=[{"check_runs": []}, statuses]):
            self.assertEqual(mr.check_states("o/r", SHA)["ci"], "success")


class Disputed(unittest.TestCase):
    def test_dispute_after_hold(self):
        cs = [comment(body("HOLD"), "2026-11-07T09:00:00Z"),
              comment("Dispute: the test is fine", "2026-11-07T10:00:00Z")]
        self.assertTrue(mr.disputed(cs))

    def test_dispute_before_hold_or_no_hold_is_not_disputed(self):
        self.assertFalse(mr.disputed([comment("dispute: x", "2026-11-07T08:00:00Z"),
                                      comment(body("HOLD"), "2026-11-07T09:00:00Z")]))
        self.assertFalse(mr.disputed([comment("dispute: x")]))
        self.assertFalse(mr.disputed([comment(body("HOLD")), comment("thanks", "2026-11-07T11:00:00Z")]))


class RepairReasons(unittest.TestCase):
    REPO = "o/r"

    def run_repair(self, pr, comments=(), states=None, detail=None, pushed="2026-11-07T08:00:00Z", extra_prs=()):
        commit = {"commit": {"committer": {"date": pushed}}}

        def fake_api(path):
            if path.startswith(f"repos/{self.REPO}/pulls?"):
                return [pr, *extra_prs]
            if "/commits/" in path:
                return commit
            if "/issues/" in path:
                return list(comments)
            if "/pulls/" in path:
                return detail or {}
            raise AssertionError(path)

        with mock.patch.object(mr, "api", side_effect=fake_api), \
             mock.patch.object(mr, "check_states", return_value=states or {}):
            return mr.repair_candidates(self.REPO, {"macos"}, NOW)

    @staticmethod
    def pr(base="main", ref="feat", n=1):
        return {"number": n, "draft": False, "labels": [], "head": {"sha": SHA, "ref": ref}, "base": {"ref": base}}

    def test_hold(self):
        out = self.run_repair(self.pr(), [comment(body("HOLD"))])
        self.assertEqual(out[0][3], "HOLD")

    def test_failed(self):
        out = self.run_repair(self.pr(), states={"macos": "failure"})
        self.assertEqual(out[0][3], "FAILED:macos")

    def test_conflict(self):
        out = self.run_repair(self.pr(), states={"macos": "success"}, detail={"mergeable_state": "dirty"})
        self.assertEqual(out[0][3], "CONFLICT")

    def test_stack_base_merged(self):
        out = self.run_repair(self.pr(base="gone-branch"))
        self.assertEqual(out[0][3], "STACK-BASE-MERGED")

    def test_stack_base_still_open_is_not_listed(self):
        base = self.pr(ref="gone-branch", n=2)
        self.assertEqual(self.run_repair(self.pr(base="gone-branch"), extra_prs=[base]), [])

    def test_fresh_push_and_live_claim_are_skipped(self):
        self.assertEqual(self.run_repair(self.pr(), [comment(body("HOLD"))], pushed="2026-11-07T11:50:00Z"), [])
        claim = comment("repair-claim: m1 until 2026-11-07T13:00Z")
        self.assertEqual(self.run_repair(self.pr(), [comment(body("HOLD")), claim]), [])

    def test_healthy_pr_not_listed(self):
        self.assertEqual(self.run_repair(self.pr(), states={"macos": "success"},
                                         detail={"mergeable_state": "clean"}), [])


if __name__ == "__main__":
    unittest.main()

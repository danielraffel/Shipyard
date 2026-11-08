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


def node(n=1, base="main", ref="feat", sha=SHA, draft=False, labels=(), states=None, comments=(),
         pushed="2026-11-07T08:00:00Z", merge="CLEAN", created="2026-11-07T07:00:00Z", statuses=(), runs=None):
    """A pullRequests node exactly as the GraphQL query returns it."""
    ctx = [{"__typename": "CheckRun", "name": k, "status": "COMPLETED", "conclusion": v.upper(), "databaseId": 100 + i}
           for i, (k, v) in enumerate((states or {}).items())] + list(runs or []) + list(statuses)
    return {"id": f"PR_{n}", "number": n, "isDraft": draft, "baseRefName": base, "headRefName": ref,
            "headRefOid": sha, "createdAt": created, "mergeStateStatus": merge,
            "labels": {"nodes": [{"name": l} for l in labels]},
            "comments": {"totalCount": len(comments), "nodes": [
                {"body": c["body"], "createdAt": c["created_at"],
                 "author": {"login": "x", "__typename": "User"}} for c in comments]},
            "commits": {"nodes": [{"commit": {"committedDate": pushed, "statusCheckRollup": {
                "contexts": {"pageInfo": {"hasNextPage": False, "endCursor": None}, "nodes": ctx}}}}]}}


def serve(repo_nodes, pages=1):
    """Patch the GraphQL transport with fixture pages for one repo."""
    chunks = [repo_nodes[i::pages] for i in range(pages)] if pages > 1 else [repo_nodes]
    calls = iter(range(pages))

    def fake(repo, query, **vars):
        i = next(calls)
        return {"repository": {"pullRequests": {"nodes": chunks[i],
                "pageInfo": {"hasNextPage": i < pages - 1, "endCursor": f"c{i}"}}}}
    mr._PRS.clear()
    return mock.patch.object(mr, "graphql", side_effect=fake)


class CheckStates(unittest.TestCase):
    REPO = "o/r"

    def states(self, **kw):
        with serve([node(**kw)]):
            mr.fetch_prs(self.REPO)
        return mr.check_states(self.REPO, SHA)

    def test_orders_by_id_so_stale_later_timestamp_failure_loses(self):
        runs = [
            {"__typename": "CheckRun", "name": "macos", "status": "COMPLETED", "conclusion": "FAILURE", "databaseId": 10},
            {"__typename": "CheckRun", "name": "macos", "status": "COMPLETED", "conclusion": "SUCCESS", "databaseId": 20},
            {"__typename": "CheckRun", "name": "lint", "status": "IN_PROGRESS", "conclusion": None, "databaseId": 30},
        ]
        st = self.states(runs=runs)
        self.assertEqual(st["macos"], "success")
        self.assertEqual(st["lint"], "pending")

    def test_status_contexts_override_runs_and_newest_wins(self):
        statuses = [
            {"__typename": "StatusContext", "context": "ci", "state": "SUCCESS", "createdAt": "2026-11-07T10:00:00Z"},
            {"__typename": "StatusContext", "context": "ci", "state": "FAILURE", "createdAt": "2026-11-07T09:00:00Z"},
            {"__typename": "StatusContext", "context": "macos", "state": "FAILURE", "createdAt": "2026-11-07T09:00:00Z"},
        ]
        st = self.states(states={"macos": "success"}, statuses=statuses)
        self.assertEqual(st["ci"], "success")
        self.assertEqual(st["macos"], "failure")

    def test_rest_fallback_matches_graphql_fold(self):
        runs = {"check_runs": [
            {"id": 20, "name": "macos", "status": "completed", "conclusion": "success"},
            {"id": 10, "name": "macos", "status": "completed", "conclusion": "failure"}]}
        mr._PRS.clear()
        with mock.patch.object(mr, "api", side_effect=[runs, {"statuses": []}]):
            self.assertEqual(mr.check_states("o/r", "d" * 40), {"macos": "success"})

    def test_rollup_contexts_paginate(self):
        n = node(states={"a": "success"})
        n["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]["pageInfo"] = {"hasNextPage": True, "endCursor": "k"}
        more = {"node": {"commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {
            "pageInfo": {"hasNextPage": False, "endCursor": None},
            "nodes": [{"__typename": "CheckRun", "name": "b", "status": "COMPLETED",
                       "conclusion": "FAILURE", "databaseId": 999}]}}}}]}}}
        mr._PRS.clear()
        first = {"repository": {"pullRequests": {"nodes": [n], "pageInfo": {"hasNextPage": False, "endCursor": None}}}}
        with mock.patch.object(mr, "graphql", side_effect=[first, more]):
            mr.fetch_prs("o/r")
        self.assertEqual(mr.check_states("o/r", SHA), {"a": "success", "b": "failure"})


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

    def run_repair(self, pr, extra_prs=()):
        with serve([pr, *extra_prs]):
            return mr.repair_candidates(self.REPO, {"macos"}, NOW)

    @staticmethod
    def pr(**kw):
        return node(**kw)

    def setUp(self):
        mr._PRS.clear()

    def test_hold(self):
        out = self.run_repair(self.pr(comments=[comment(body("HOLD"))]))
        self.assertEqual(out[0][3], "HOLD")

    def test_failed(self):
        out = self.run_repair(self.pr(states={"macos": "failure"}))
        self.assertEqual(out[0][3], "FAILED:macos")

    def test_conflict(self):
        out = self.run_repair(self.pr(states={"macos": "success"}, merge="DIRTY"))
        self.assertEqual(out[0][3], "CONFLICT")

    def test_stack_base_merged(self):
        out = self.run_repair(self.pr(base="gone-branch"))
        self.assertEqual(out[0][3], "STACK-BASE-MERGED")

    def test_stack_base_still_open_is_not_listed(self):
        base = self.pr(ref="gone-branch", n=2, states={"macos": "success"})
        self.assertEqual(self.run_repair(self.pr(base="gone-branch"), extra_prs=[base]), [])

    def test_fresh_push_and_live_claim_are_skipped(self):
        hold = [comment(body("HOLD"))]
        self.assertEqual(self.run_repair(self.pr(comments=hold, pushed="2026-11-07T11:50:00Z")), [])
        claim = comment("repair-claim: m1 until 2026-11-07T13:00Z")
        self.assertEqual(self.run_repair(self.pr(comments=hold + [claim])), [])

    def test_healthy_pr_not_listed(self):
        self.assertEqual(self.run_repair(self.pr(states={"macos": "success"})), [])

    def test_hold_label_and_draft_skipped(self):
        hold = [comment(body("HOLD"))]
        self.assertEqual(self.run_repair(self.pr(comments=hold, labels=["owner-decision"])), [])
        self.assertEqual(self.run_repair(self.pr(comments=hold, labels=["PARKED/windows"])), [])
        self.assertEqual(self.run_repair(self.pr(comments=hold, draft=True)), [])


class ReviewCandidates(unittest.TestCase):
    REPO = "o/r"

    def run_review(self, *nodes):
        with serve(list(nodes)), mock.patch.object(mr, "overrides", return_value={}):
            return mr.review_candidates(self.REPO, {"macos"}, NOW)

    def test_green_without_verdict_listed_and_disputed_marked(self):
        green = {"macos": "success"}
        out = self.run_review(node(1, states=green),
                              node(2, states=green, sha="2" * 40, comments=[comment(body("HOLD", sha=OLD), "2026-11-07T09:00:00Z"),
                                                              comment("dispute: no", "2026-11-07T10:00:00Z")]))
        self.assertEqual([(o[0], o[3]) for o in out], [("o/r#1", ""), ("o/r#2", "DISPUTED")])

    def test_verdict_red_stack_label_and_draft_excluded(self):
        green = {"macos": "success"}
        out = self.run_review(
            node(1, states=green, comments=[comment(body())]),
            node(2, states={"macos": "failure"}, sha="2" * 40),
            node(3, states=green, base="other", sha="3" * 40),
            node(4, states=green, labels=["superseded"], sha="4" * 40),
            node(5, states=green, draft=True, sha="5" * 40))
        self.assertEqual(out, [])

    def test_override_lists_pr_until_newer_verdict(self):
        since = datetime.datetime(2026, 11, 7, 9, 0, tzinfo=datetime.timezone.utc)
        ov = {"o/r#1": (SHA, since)}
        for cs, expect in (([comment(body(), "2026-11-07T08:00:00Z")], 1), ([comment(body(), "2026-11-07T10:00:00Z")], 0)):
            with serve([node(1, states={"macos": "failure"}, comments=cs)]), \
                 mock.patch.object(mr, "overrides", return_value=ov):
                out = mr.review_candidates(self.REPO, {"macos"}, NOW)
            self.assertEqual(len(out), expect)
            if expect:
                self.assertEqual(out[0][3], "AUTHOR-SELF-APPROVED")

    def test_more_than_100_comments_falls_back_to_rest(self):
        n = node(1, states={"macos": "success"}, comments=[comment("x")])
        n["comments"]["totalCount"] = 150
        rest = [{"body": body(), "created_at": "2026-11-07T10:00:00Z", "user": {"login": "x", "type": "User"}}]
        with serve([n]), mock.patch.object(mr, "api", return_value=rest) as api:
            out = mr.review_candidates(self.REPO, {"macos"}, NOW)
        self.assertEqual(out, [])
        api.assert_called_once()


class ApiBudget(unittest.TestCase):
    """A run costs 2 REST reads (required) plus one GraphQL page per 30 open PRs, per repo."""

    def test_calls_per_repo_are_bounded_regardless_of_pr_count(self):
        nodes = [node(i, states={"macos": "success"}, comments=[comment("hi")]) for i in range(1, 46)]
        pages = [{"data": {"repository": {"pullRequests": {
            "nodes": nodes[:30], "pageInfo": {"hasNextPage": True, "endCursor": "a"}}}}},
            {"data": {"repository": {"pullRequests": {
                "nodes": nodes[30:], "pageInfo": {"hasNextPage": False, "endCursor": None}}}}}]
        calls = {"rest": 0, "graphql": 0}

        def fake_run(cmd, **kw):
            if cmd[2] == "graphql":
                doc = pages[calls["graphql"]]
                calls["graphql"] += 1
                return mock.Mock(returncode=0, stdout=json.dumps(doc), stderr="")
            calls["rest"] += 1
            body_ = "{}" if "protection" in cmd[-1] else "[]"
            return mock.Mock(returncode=0, stdout=body_, stderr="")

        for mode in (mr.review_candidates, mr.repair_candidates):
            mr._PRS.clear()
            calls.update(rest=0, graphql=0)
            with mock.patch.object(mr.subprocess, "run", side_effect=fake_run), \
                 mock.patch.object(mr, "overrides", return_value={}):
                req = mr.required("o/r")
                mode("o/r", req, NOW)
            self.assertEqual(calls["graphql"], 2)
            self.assertLessEqual(calls["rest"], 3)

    def test_graphql_failure_raises_so_runner_exits_2(self):
        mr._PRS.clear()
        bad = mock.Mock(returncode=1, stdout="", stderr="API rate limit exceeded")
        with mock.patch.object(mr.subprocess, "run", return_value=bad):
            with self.assertRaises(RuntimeError):
                mr.fetch_prs("o/r")
        mr._PRS.clear()
        errs = mock.Mock(returncode=0, stdout=json.dumps({"errors": [{"message": "boom"}], "data": None}), stderr="")
        with mock.patch.object(mr.subprocess, "run", return_value=errs):
            with self.assertRaises(RuntimeError):
                mr.fetch_prs("o/r")


if __name__ == "__main__":
    unittest.main()

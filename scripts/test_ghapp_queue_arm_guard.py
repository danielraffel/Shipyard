from __future__ import annotations

import contextlib
import copy
import importlib.util
import io
import json
import os
import pathlib
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from typing import Any
from unittest import mock


SCRIPT = pathlib.Path(__file__).with_name("ghapp_queue_arm_guard.py")
SPEC = importlib.util.spec_from_file_location("ghapp_queue_arm_guard", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)

FIXTURES = pathlib.Path(__file__).resolve().parent.parent / "tests" / "fixtures" / "github"


def review_fixture(name: str) -> Any:
    return json.loads((FIXTURES / "review_before_arm" / name).read_text(encoding="utf-8"))


def fixture(name: str) -> dict[str, Any]:
    return json.loads((FIXTURES / name).read_text(encoding="utf-8"))


def expectations() -> dict[str, Any]:
    return {
        name: value
        for name, value in fixture("expected_classifications.json").items()
        if not name.startswith("_")
    }


def with_base(response: dict[str, Any], base: str = "main") -> dict[str, Any]:
    response = copy.deepcopy(response)
    response["data"]["repository"]["pullRequest"]["baseRefName"] = base
    return response


def as_node(response: dict[str, Any]) -> dict[str, Any]:
    return {"data": {"node": response["data"]["repository"]["pullRequest"]}}


class ClassifierAgreesWithSharedCorpus(unittest.TestCase):
    """The Python twin must agree with src/pr_queue_state.rs on real responses."""

    def test_every_fixture_matches_the_shared_expectations(self) -> None:
        checked = 0
        for name, want in expectations().items():
            with self.subTest(fixture=name):
                got = guard.classify_pr_queue_state(fixture(name))
                self.assertEqual(got["class"], want["class"])
                self.assertEqual(got["pr"], want["pr"])
                self.assertEqual(got["requeues_without_new_head"], want["requeues_without_new_head"])
                ejection = got["last_ejection"]
                self.assertEqual(ejection and ejection["reason"], want["last_ejection_reason"])
                if "last_ejection_new_head_since" in want:
                    self.assertEqual(ejection["new_head_since"], want["last_ejection_new_head_since"])
                if "ejections_of_current_head" in want:
                    self.assertEqual(
                        got["ejections_of_current_head"], want["ejections_of_current_head"]
                    )
                if "merge_group_commit" in want:
                    self.assertEqual(ejection["merge_group_commit"], want["merge_group_commit"])
                for key in ("entry_state", "position", "enabled_at", "reason", "new_head_since_removal"):
                    if key in want:
                        self.assertEqual(got[key], want[key])
                allowed, _ = guard.decide(got)
                self.assertEqual("allow" if allowed else "refuse", want["guard"])
                checked += 1
        # Control: the whole corpus, real and labelled-synthetic, was visited.
        self.assertEqual(checked, 16)

    def test_real_truncated_same_head_ejection_is_ejected_and_refused(self) -> None:
        response = fixture("pr_real_truncated_same_head_ejected.json")
        provenance = response["_provenance"]
        self.assertEqual(provenance["source_pr"], "Generous-Corp/pulp#8638")
        self.assertEqual(provenance["cut_event_created_at"], "2026-09-22T10:26:42Z")
        got = guard.classify_pr_queue_state(response)
        self.assertEqual(got["class"], "ejected")
        self.assertEqual(got["reason"], "failed_checks")
        self.assertIs(got["new_head_since_removal"], False)
        self.assertEqual(got["requeues_without_new_head"], 3)
        allowed, _ = guard.decide(got)
        self.assertFalse(allowed)

    def test_synthetic_fixtures_are_labelled(self) -> None:
        for name, want in expectations().items():
            with self.subTest(fixture=name):
                self.assertEqual(bool(want.get("synthetic")), "_synthetic" in fixture(name))
                self.assertEqual(bool(want.get("synthetic")), "synthetic" in name)
                self.assertEqual(bool(want.get("real_truncated")), "_provenance" in fixture(name))

    def test_merged_removal_is_not_an_ejection(self) -> None:
        response = fixture("pr_merged.json")
        response["data"]["repository"]["pullRequest"]["state"] = "OPEN"
        self.assertEqual(guard.classify_pr_queue_state(response)["class"], "never_armed")

    def test_ejected_without_and_with_new_head(self) -> None:
        response = fixture("pr_ejected_requeued.json")
        pr = response["data"]["repository"]["pullRequest"]
        pr["isInMergeQueue"] = False
        pr["mergeQueueEntry"] = None
        pr["timelineItems"]["nodes"].pop()
        got = guard.classify_pr_queue_state(response)
        self.assertEqual(got["class"], "ejected")
        self.assertFalse(got["new_head_since_removal"])
        # A force-push that names no commit is not evidence of a new head.
        pr["timelineItems"]["nodes"].append({"__typename": "HeadRefForcePushedEvent"})
        self.assertFalse(guard.classify_pr_queue_state(response)["new_head_since_removal"])
        # One that put the current head on the branch is.
        pr["headRefOid"] = PUSHED_HEAD
        pr["timelineItems"]["nodes"].append(
            {"__typename": "HeadRefForcePushedEvent", "afterCommit": {"oid": PUSHED_HEAD}}
        )
        self.assertTrue(guard.classify_pr_queue_state(response)["new_head_since_removal"])

    def test_unreadable_is_unknown(self) -> None:
        for value in ({}, {"errors": [{"message": "x"}]}, {"data": {"node": {"state": "OPEN"}}}):
            with self.subTest(value=value):
                self.assertEqual(guard.classify_pr_queue_state(value)["class"], "unknown")


PUSHED_HEAD = "1" * 40
BACKDATED_FIXTURE = "pr_real_8912_backdated_fix_after_ejection.json"
# The head the queue removed from 8912: beforeCommit's second parent.
REMOVED_8912 = "f4b97cb8e4458072e2ff10429c2361ec3b446b5e"
# The back-dated fix pushed after the ejection.
FIX_8912 = "c246e05b54069e800a8d1e5f8f4c9d5e841818d7"


def _removal_index(nodes: list[dict[str, Any]]) -> int:
    return max(i for i, n in enumerate(nodes) if n["__typename"] == "RemovedFromMergeQueueEvent")


MERGE_CONFLICT_FIXTURE = "pr_real_9653_merge_conflict_backdated_push.json"
# 9653's head after the ejection: committed before it, pushed after it.
PUSHED_9653 = "99403d83614a0e535a3d2a5bac391e5ab326b639"


class MergeConflictHeadIsDecidedByFirstSuite(unittest.TestCase):
    """Twin of the Rust merge_conflict push-time tests."""

    def setUp(self) -> None:
        self.response = fixture(MERGE_CONFLICT_FIXTURE)
        self.nodes = self.response["data"]["repository"]["pullRequest"]["timelineItems"]["nodes"]

    def set_head_suites(self, suites: Any) -> None:
        for node in self.nodes:
            if node.get("commit", {}).get("oid") == PUSHED_9653:
                node["commit"]["checkSuites"] = suites

    def test_a_head_pushed_after_a_merge_conflict_ejection_is_new_by_its_first_suite(self) -> None:
        self.assertEqual(self.response["_provenance"]["source_pr"], "Generous-Corp/pulp#9653")
        # Control: the removal names no head and GitHub sorts the pushed
        # commit BEFORE it, so neither the SHA nor the timeline can answer.
        pushed = next(i for i, n in enumerate(self.nodes) if n.get("commit", {}).get("oid") == PUSHED_9653)
        self.assertLess(pushed, _removal_index(self.nodes))
        self.assertIsNone(self.nodes[_removal_index(self.nodes)].get("beforeCommit"))
        got = guard.classify_pr_queue_state(self.response)
        self.assertIs(got["new_head_since_removal"], True)
        self.assertEqual(got["last_ejection"]["new_head_basis"], "suite_after_removal")
        allowed, message = guard.decide(got)
        self.assertTrue(allowed, message)

    def test_a_head_whose_first_suite_predates_the_ejection_is_not_new(self) -> None:
        self.set_head_suites({"nodes": [{"createdAt": "2026-10-05T23:58:00Z"}]})
        got = guard.classify_pr_queue_state(self.response)
        self.assertIs(got["new_head_since_removal"], False)
        self.assertEqual(got["last_ejection"]["new_head_basis"], "no_evidence")
        self.assertFalse(guard.decide(got)[0])

    def test_a_head_with_no_check_suite_keeps_the_timeline_rule(self) -> None:
        for suites in ({"nodes": []}, None):
            with self.subTest(suites=suites):
                self.setUp()
                self.set_head_suites(suites)
                got = guard.classify_pr_queue_state(self.response)
                self.assertIs(got["new_head_since_removal"], False)
                self.assertFalse(guard.decide(got)[0])


class NewHeadIsDecidedBySha(unittest.TestCase):
    """Twin of the Rust ``pr_queue_state`` new-head tests."""

    def setUp(self) -> None:
        self.response = fixture(BACKDATED_FIXTURE)
        self.pr = self.response["data"]["repository"]["pullRequest"]
        self.nodes = self.pr["timelineItems"]["nodes"]

    def classify(self) -> dict[str, Any]:
        return guard.classify_pr_queue_state(self.response)

    def test_backdated_fix_pushed_after_the_ejection_is_a_new_head(self) -> None:
        self.assertEqual(self.response["_provenance"]["source_pr"], "Generous-Corp/pulp#8912")
        # Control: GitHub sorts the back-dated fix BEFORE the removal, so
        # timeline position alone would say "no new head".
        fix = next(i for i, n in enumerate(self.nodes) if n.get("commit", {}).get("oid") == FIX_8912)
        self.assertLess(fix, _removal_index(self.nodes))
        got = self.classify()
        self.assertEqual(got["class"], "ejected")
        self.assertIs(got["new_head_since_removal"], True)
        self.assertEqual(got["last_ejection"]["removed_head"], REMOVED_8912)
        self.assertEqual(got["last_ejection"]["new_head_basis"], "removed_head_sha")
        allowed, message = guard.decide(got)
        self.assertTrue(allowed, message)
        self.assertIn("new head since", message)

    def test_the_head_the_queue_removed_is_still_the_same_head(self) -> None:
        self.pr["headRefOid"] = REMOVED_8912
        got = self.classify()
        self.assertIs(got["new_head_since_removal"], False)
        self.assertFalse(guard.decide(got)[0])
        # Equal SHAs are the same head whatever the timeline says.
        self.nodes.append(
            {"__typename": "HeadRefForcePushedEvent", "afterCommit": {"oid": REMOVED_8912}}
        )
        self.assertIs(self.classify()["new_head_since_removal"], False)

    def test_force_push_to_an_older_sha_is_a_new_head(self) -> None:
        older = "0a" * 20
        self.pr["headRefOid"] = older
        self.nodes.append({"__typename": "HeadRefForcePushedEvent", "afterCommit": {"oid": older}})
        got = self.classify()
        self.assertEqual(got["last_ejection"]["removed_head"], REMOVED_8912)
        self.assertIs(got["new_head_since_removal"], True)

    def test_missing_before_commit_falls_back_to_pushes_and_fails_closed(self) -> None:
        del self.nodes[_removal_index(self.nodes)]["beforeCommit"]
        got = self.classify()
        self.assertIsNone(got["last_ejection"]["removed_head"])
        self.assertEqual(got["last_ejection"]["new_head_basis"], "no_evidence")
        self.assertIs(got["new_head_since_removal"], False)
        self.assertFalse(guard.decide(got)[0])
        # A later push of some other commit is still not the current head.
        self.nodes.append({"__typename": "PullRequestCommit", "commit": {"oid": PUSHED_HEAD}})
        self.assertIs(self.classify()["new_head_since_removal"], False)
        # A later push of the current head is.
        self.pr["headRefOid"] = PUSHED_HEAD
        got = self.classify()
        self.assertEqual(got["last_ejection"]["new_head_basis"], "push_after_removal")
        self.assertIs(got["new_head_since_removal"], True)

    def test_a_merge_group_parent_this_pr_never_had_is_not_trusted(self) -> None:
        index = _removal_index(self.nodes)
        self.nodes[index]["beforeCommit"]["parents"]["nodes"][1]["oid"] = PUSHED_HEAD
        got = self.classify()
        self.assertIsNone(got["last_ejection"]["removed_head"])
        self.assertIs(got["new_head_since_removal"], False)

    def test_requeue_count_compares_removed_heads_not_timeline_position(self) -> None:
        a, b = REMOVED_8912, FIX_8912

        def removal(head: str) -> dict[str, Any]:
            return {
                "__typename": "RemovedFromMergeQueueEvent",
                "reason": "failed_checks",
                "createdAt": "2026-09-27T05:52:04Z",
                "beforeCommit": {
                    "oid": "ffff",
                    "parents": {"nodes": [{"oid": "base"}, {"oid": head}]},
                },
            }

        def build(second: str) -> dict[str, Any]:
            return {"data": {"repository": {"pullRequest": {
                "number": 1, "state": "OPEN", "headRefOid": b,
                "isInMergeQueue": False, "mergeQueueEntry": None, "autoMergeRequest": None,
                "timelineItems": {"pageInfo": {"hasPreviousPage": False}, "nodes": [
                    {"__typename": "PullRequestCommit", "commit": {"oid": a}},
                    {"__typename": "AddedToMergeQueueEvent", "createdAt": "t0"},
                    {"__typename": "PullRequestCommit", "commit": {"oid": b}},
                    removal(a),
                    {"__typename": "AddedToMergeQueueEvent", "createdAt": "t1"},
                    removal(second),
                ]},
            }}}}

        self.assertEqual(guard.classify_pr_queue_state(build(b))["requeues_without_new_head"], 0)
        # Control: the second removal naming A again is a same-head re-enqueue.
        self.assertEqual(guard.classify_pr_queue_state(build(a))["requeues_without_new_head"], 1)

    def test_query_selects_the_removed_head_and_force_push_target(self) -> None:
        self.assertIn("beforeCommit{oid parents(first:3){nodes{oid}}}", guard.PR_BY_NUMBER_QUERY)
        self.assertIn("HeadRefForcePushedEvent{createdAt afterCommit{oid}}", guard.PR_BY_NUMBER_QUERY)


def stub_review(case: unittest.TestCase) -> None:
    """Allow every review: these cases decide queue state; ReviewBeforeArm decides review."""
    patcher = mock.patch.object(
        guard, "review_gate", return_value=(True, "reviewed (stubbed for queue-state cases)", None)
    )
    patcher.start()
    case.addCleanup(patcher.stop)


class QueueArmGuardTests(unittest.TestCase):
    def setUp(self) -> None:
        # Run from a directory with no `.shipyard/config.toml` above it, so these
        # cases never pick up a batch attributor from the checkout they run in
        # and their exact gh-call lists stay meaningful.
        directory = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, directory, True)
        previous = pathlib.Path.cwd()
        os.chdir(directory)
        self.addCleanup(os.chdir, previous)
        stub_review(self)

    def run_guard(
        self, args: list[str], responses: list[Any] | None = None, **env: str
    ) -> tuple[int, str, list[list[str]]]:
        calls: list[list[str]] = []
        queue = list(responses or [])

        def fake_gh(arguments: list[str]) -> Any:
            calls.append(arguments)
            if not queue:
                raise AssertionError(f"unexpected gh call {arguments}")
            value = queue.pop(0)
            if isinstance(value, Exception):
                raise value
            return value

        stderr = io.StringIO()
        base_env = {"GH_REPO": "Generous-Corp/pulp"}
        base_env.update(env)
        with (
            mock.patch.dict(os.environ, base_env, clear=True),
            mock.patch.object(guard, "run_real_gh", side_effect=fake_gh),
            contextlib.redirect_stderr(stderr),
        ):
            code = guard.main(args)
        return code, stderr.getvalue(), calls

    def test_harmless_commands_make_no_live_reads(self) -> None:
        for args in (
            ["pr", "view", "7"],
            ["pr", "merge", "7", "--disable-auto"],
            ["api", "graphql", "-f", "query=query { viewer { login } }"],
            ["api", "repos/o/r/pulls/7"],
            ["pr", "merge", "--help"],
        ):
            with self.subTest(args=args):
                code, _, calls = self.run_guard(args)
                self.assertEqual(code, 0)
                self.assertEqual(calls, [])

    def test_queued_pr_is_refused_with_position_and_rest_note(self) -> None:
        code, message, calls = self.run_guard(
            ["pr", "merge", "8669", "--auto", "--merge"], [fixture("pr_queued.json")]
        )
        self.assertEqual(code, 1)
        self.assertIn("already in the merge queue at position 1", message)
        self.assertIn("REST auto_merge=null is expected", message)
        self.assertIn("`shipyard landing --pr <n>`", message)
        self.assertIn("number=8669", calls[0])

    def test_re_enqueued_queued_pr_is_refused(self) -> None:
        code, message, _ = self.run_guard(
            ["pr", "merge", "8702", "--auto"], [fixture("pr_ejected_requeued.json")]
        )
        self.assertEqual(code, 1)
        self.assertIn("position 3", message)

    def test_armed_pr_is_refused(self) -> None:
        code, message, _ = self.run_guard(
            ["pr", "merge", "8678", "--auto"], [fixture("pr_armed_not_queued.json")]
        )
        self.assertEqual(code, 1)
        self.assertIn("already armed since 2026-09-21T23:10:04Z", message)
        self.assertIn("queue will pick it up", message)

    def test_same_head_ejection_is_refused_with_the_correct_path(self) -> None:
        code, message, _ = self.run_guard(
            ["pr", "merge", "8638", "--auto"], [fixture("pr_real_truncated_same_head_ejected.json")]
        )
        self.assertEqual(code, 1)
        self.assertIn("ejected for failed_checks at 2026-09-22T10:26:42Z", message)
        self.assertIn("Push a fix first, then `shipyard ship --pr 8638`", message)
        self.assertIn("see docs/ghapp-guards.md", message)

    def test_manual_same_head_removal_is_refused_with_reason_wording(self) -> None:
        response = fixture("pr_ejected_new_head.json")
        pr = response["data"]["repository"]["pullRequest"]
        nodes = pr["timelineItems"]["nodes"]
        while nodes[-1]["__typename"] == "PullRequestCommit":
            nodes.pop()
        code, message, _ = self.run_guard(["pr", "merge", "8722", "--auto"], [response])
        self.assertEqual(code, 1)
        self.assertIn(
            "removed from the queue (manual) at 2026-09-22T21:30:26Z", message
        )
        self.assertIn("confirm with whoever dequeued it", message)
        self.assertNotIn("ALLGREEN", message)

    def test_invalid_merge_commit_same_head_is_allowed(self) -> None:
        response = fixture("pr_real_truncated_same_head_ejected.json")
        pr = response["data"]["repository"]["pullRequest"]
        for node in pr["timelineItems"]["nodes"]:
            if node["__typename"] == "RemovedFromMergeQueueEvent":
                node["reason"] = "invalid_merge_commit"
        code, _, _ = self.run_guard(["pr", "merge", "8638", "--auto"], [response])
        self.assertEqual(code, 0)

    def test_refusal_text_never_names_bypass_or_override_variables(self) -> None:
        refusals = [
            (["pr", "merge", "8669", "--auto"], [fixture("pr_queued.json")]),
            (["pr", "merge", "8678", "--auto"], [fixture("pr_armed_not_queued.json")]),
            (["pr", "merge", "8638", "--auto"], [fixture("pr_real_truncated_same_head_ejected.json")]),
            (["pr", "merge", "8721", "--auto"], [fixture("pr_merged.json")]),
            (["pr", "merge", "1", "--auto"], [fixture("pr_synthetic_truncated_window.json")]),
            (["pr", "merge", "1", "--auto"], [guard.GuardError("HTTP 502")]),
            (["api", "graphql", "--input", "-"], []),
        ]
        for args, responses in refusals:
            with self.subTest(args=args):
                code, message, _ = self.run_guard(args, responses)
                self.assertEqual(code, 1)
                self.assertIn("docs/ghapp-guards.md", message)
                for name in ("GHAPP_ALLOW_QUEUE_REARM", "SHIPYARD_INTERNAL_QUEUE_MUTATION"):
                    self.assertNotIn(name, message)

    def test_truncated_window_is_refused_as_unknown(self) -> None:
        code, message, _ = self.run_guard(
            ["pr", "merge", "900001", "--auto"], [fixture("pr_synthetic_truncated_window.json")]
        )
        self.assertEqual(code, 1)
        self.assertIn("truncated", message)

    def test_ejection_with_new_head_is_allowed(self) -> None:
        response = fixture("pr_ejected_requeued.json")
        pr = response["data"]["repository"]["pullRequest"]
        pr["isInMergeQueue"] = False
        pr["mergeQueueEntry"] = None
        pr["headRefOid"] = "f" * 40
        pr["timelineItems"]["nodes"][-1] = {
            "__typename": "PullRequestCommit",
            "commit": {"oid": "f" * 40},
        }
        code, _, _ = self.run_guard(["pr", "merge", "8702", "--auto"], [response])
        self.assertEqual(code, 0)

    def test_never_armed_is_allowed(self) -> None:
        code, _, _ = self.run_guard(
            ["pr", "merge", "8672", "--auto", "--merge"], [fixture("pr_never_armed.json")]
        )
        self.assertEqual(code, 0)

    def test_merged_and_unknown_are_refused(self) -> None:
        code, message, _ = self.run_guard(
            ["pr", "merge", "8721", "--auto"], [fixture("pr_merged.json")]
        )
        self.assertEqual(code, 1)
        self.assertIn("is merged", message)
        code, message, _ = self.run_guard(
            ["pr", "merge", "1", "--auto"], [guard.GuardError("HTTP 502")]
        )
        self.assertEqual(code, 1)
        self.assertIn("HTTP 502", message)
        code, message, _ = self.run_guard(["pr", "merge", "1", "--auto"], [{"data": None}])
        self.assertEqual(code, 1)
        self.assertIn("could not be determined", message)

    def test_internal_marker_bypasses_without_reading(self) -> None:
        code, _, calls = self.run_guard(
            ["pr", "merge", "8669", "--auto"], SHIPYARD_INTERNAL_QUEUE_MUTATION="1"
        )
        self.assertEqual(code, 0)
        self.assertEqual(calls, [])

    def test_override_allows_with_warning(self) -> None:
        code, message, _ = self.run_guard(
            ["pr", "merge", "8669", "--auto"],
            [fixture("pr_queued.json")],
            GHAPP_ALLOW_QUEUE_REARM="1",
        )
        self.assertEqual(code, 0)
        self.assertIn("WARNING", message)

    def test_plain_merge_on_queue_branch_is_classified(self) -> None:
        code, message, calls = self.run_guard(
            ["pr", "merge", "8669", "--merge"],
            [with_base(fixture("pr_queued.json")), {"data": {"repository": {"mergeQueue": {"id": "MQ"}}}}],
        )
        self.assertEqual(code, 1)
        self.assertIn("already in the merge queue", message)
        self.assertIn("base=main", calls[1])

    def test_plain_merge_without_a_queue_is_not_an_enqueue(self) -> None:
        code, _, _ = self.run_guard(
            ["pr", "merge", "8678", "--merge"],
            [with_base(fixture("pr_armed_not_queued.json")), {"data": {"repository": {"mergeQueue": None}}}],
        )
        self.assertEqual(code, 0)

    def test_url_selector_names_repo_and_number(self) -> None:
        code, _, calls = self.run_guard(
            ["pr", "merge", "https://github.com/Generous-Corp/pulp/pull/8669", "--auto"],
            [fixture("pr_queued.json")],
        )
        self.assertEqual(code, 1)
        self.assertIn("owner=Generous-Corp", calls[0])
        self.assertIn("number=8669", calls[0])

    def test_branch_selector_is_resolved_through_pr_view(self) -> None:
        code, _, calls = self.run_guard(
            ["pr", "merge", "feature/x", "--auto"],
            [{"number": 8672}, fixture("pr_never_armed.json")],
        )
        self.assertEqual(code, 0)
        self.assertEqual(calls[0][:3], ["pr", "view", "feature/x"])

    def test_graphql_enable_auto_merge_recipe_is_classified(self) -> None:
        mutation = (
            "query=mutation($id:ID!){enablePullRequestAutoMerge(input:{pullRequestId:$id,"
            "mergeMethod:MERGE}){pullRequest{number}}}"
        )
        code, message, calls = self.run_guard(
            ["api", "graphql", "-F", "id=PR_kwQueued", "-f", mutation],
            [as_node(fixture("pr_queued.json"))],
        )
        self.assertEqual(code, 1)
        self.assertIn("already in the merge queue", message)
        self.assertIn("id=PR_kwQueued", calls[0])

    def test_graphql_enqueue_with_inline_id_is_classified(self) -> None:
        mutation = 'query=mutation{enqueuePullRequest(input:{pullRequestId:"PR_inline"}){mergeQueueEntry{id}}}'
        code, _, calls = self.run_guard(
            ["api", "graphql", "-f", mutation], [as_node(fixture("pr_never_armed.json"))]
        )
        self.assertEqual(code, 0)
        self.assertIn("id=PR_inline", calls[0])

    def test_graphql_query_file_and_input_file_are_inspected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            query = pathlib.Path(directory) / "q.graphql"
            query.write_text(
                "mutation($pr:ID!){enqueuePullRequest(input:{pullRequestId:$pr}){mergeQueueEntry{id}}}",
                encoding="utf-8",
            )
            code, _, _ = self.run_guard(
                ["api", "graphql", "-F", f"query=@{query}", "-f", "pr=PR_x"],
                [as_node(fixture("pr_armed_not_queued.json"))],
            )
            self.assertEqual(code, 1)
            body = pathlib.Path(directory) / "body.json"
            body.write_text(
                json.dumps(
                    {
                        "query": "mutation($pr:ID!){enablePullRequestAutoMerge(input:{pullRequestId:$pr}){clientMutationId}}",
                        "variables": {"pr": "PR_y"},
                    }
                ),
                encoding="utf-8",
            )
            code, _, calls = self.run_guard(
                ["api", "graphql", "--input", str(body)], [as_node(fixture("pr_queued.json"))]
            )
            self.assertEqual(code, 1)
            self.assertIn("id=PR_y", calls[0])

    def test_graphql_stdin_bodies_are_refused_as_ambiguous(self) -> None:
        for args in (
            ["api", "graphql", "--input", "-"],
            ["api", "graphql", "--input=-"],
            ["api", "graphql", "-Fquery=@-"],
        ):
            with self.subTest(args=args):
                code, message, calls = self.run_guard(args)
                self.assertEqual(code, 1)
                self.assertIn("ambiguous", message)
                self.assertIn("stdin", message)
                self.assertEqual(calls, [])

    def test_unresolvable_mutation_variable_is_refused(self) -> None:
        mutation = "query=mutation($id:ID!){enqueuePullRequest(input:{pullRequestId:$id}){mergeQueueEntry{id}}}"
        code, message, _ = self.run_guard(["api", "graphql", "-f", mutation])
        self.assertEqual(code, 1)
        self.assertIn("$id", message)


class InstalledLayoutEndToEnd(unittest.TestCase):
    """Run the guard as ghapp does: installed names, real subprocess, fake native gh."""

    def test_installed_guard_loads_parser_and_refuses_queued_pr(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            guards = pathlib.Path(directory)
            shutil.copy(SCRIPT, guards / "queue-arm-guard")
            shutil.copy(SCRIPT.with_name("ghapp_queue_removal_guard.py"), guards / "queue-removal-guard")
            fake_gh = guards / "gh"
            fake_gh.write_text(
                "#!/bin/sh\n"
                f"printf '%s\\n' \"$*\" >> '{guards / 'calls'}'\n"
                f"cat '{FIXTURES / 'pr_queued.json'}'\n",
                encoding="utf-8",
            )
            for executable in (fake_gh, guards / "queue-arm-guard"):
                executable.chmod(executable.stat().st_mode | stat.S_IXUSR)
            env = {"PATH": "/usr/bin:/bin", "GHAPP_REAL_GH": str(fake_gh), "GH_REPO": "Generous-Corp/pulp"}
            result = subprocess.run(
                [sys.executable, str(guards / "queue-arm-guard"), "pr", "merge", "8669", "--auto"],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn("already in the merge queue at position 1", result.stderr)
            self.assertIn("number=8669", (guards / "calls").read_text(encoding="utf-8"))

            (guards / "queue-removal-guard").unlink()
            result = subprocess.run(
                [sys.executable, str(guards / "queue-arm-guard"), "pr", "merge", "8669", "--auto"],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            # Without its parser the guard fails closed on an arm-capable command...
            self.assertEqual(result.returncode, 1)
            self.assertIn("request parser", result.stderr)
            result = subprocess.run(
                [sys.executable, str(guards / "queue-arm-guard"), "pr", "view", "8669"],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            # ...and never blocks an unrelated one.
            self.assertEqual(result.returncode, 0, result.stderr)


class BatchAttributionTests(unittest.TestCase):
    """A same-head re-enqueue after `failed_checks` needs a positive certification.

    Grounded in Generous-Corp/pulp#8811, ejected 2026-09-25T04:12:51Z by
    merge_group run 36093055057. Fixtures for the pull request, the run
    listing, the ejecting run and a test-failing run are live captures.
    """

    INCIDENT = "pr_real_8811_same_head_ejected.json"
    RUN_ID = 36093055057

    def setUp(self) -> None:
        self.root = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root, True)
        cwd = pathlib.Path.cwd()
        os.chdir(self.root)
        self.addCleanup(os.chdir, cwd)
        stub_review(self)

    def declare(
        self, verdict: Any, *, exit_code: int = 0, command: Any = None, stderr: str = ""
    ) -> None:
        """Write a `.shipyard/config.toml` declaring a stub attributor."""
        (self.root / ".shipyard").mkdir(parents=True, exist_ok=True)
        script = self.root / "attribute.py"
        body = verdict if isinstance(verdict, str) else json.dumps(verdict)
        script.write_text(
            "import sys, pathlib\n"
            f"pathlib.Path({str(self.root / 'argv')!r}).write_text(' '.join(sys.argv[1:]))\n"
            f"sys.stdout.write({body!r})\n"
            f"sys.stderr.write({stderr!r})\n"
            f"raise SystemExit({exit_code})\n",
            encoding="utf-8",
        )
        declared = command if command is not None else [sys.executable, str(script)]
        rendered = declared if isinstance(declared, str) else json.dumps(declared)
        (self.root / ".shipyard" / "config.toml").write_text(
            f"[queue.attribution]\ncommand = {rendered}\n", encoding="utf-8"
        )

    def certifies(self, **overrides: Any) -> dict[str, Any]:
        verdict = {
            "run_id": self.RUN_ID,
            "implicates_head": False,
            "verdict": "infrastructure",
            "evidence": "macos failed at Install ccache (macOS) before any content built",
        }
        verdict.update(overrides)
        return verdict

    def run_guard(
        self,
        responses: list[Any],
        pr: int = 8811,
        repo: str = "Generous-Corp/pulp",
        **env: str,
    ) -> tuple[int, str, list[list[str]]]:
        calls: list[list[str]] = []
        queue = list(responses)

        def fake_gh(arguments: list[str]) -> Any:
            calls.append(arguments)
            if not queue:
                raise AssertionError(f"unexpected gh call {arguments}")
            value = queue.pop(0)
            if isinstance(value, Exception):
                raise value
            return value

        stderr = io.StringIO()
        base_env = {"GH_REPO": repo, "PATH": os.environ.get("PATH", "/usr/bin:/bin")}
        base_env.update(env)
        with (
            mock.patch.dict(os.environ, base_env, clear=True),
            mock.patch.object(guard, "run_real_gh", side_effect=fake_gh),
            mock.patch.object(guard.PARSER, "current_repo_identity", return_value=tuple(repo.split("/"))),
            contextlib.redirect_stderr(stderr),
        ):
            code = guard.main(["pr", "merge", str(pr), "--auto"])
        return code, stderr.getvalue(), calls

    def batch_reads(self, run: str = "merge_group_run_real_infra_and_build.json") -> list[Any]:
        """The two live reads the resolver makes: the run listing, then jobs."""
        return [fixture("merge_group_failed_runs_listing.json"), fixture(run)["jobs"]]

    # -- the incident ------------------------------------------------------

    def test_incident_fixture_is_the_state_the_guard_saw(self) -> None:
        response = fixture(self.INCIDENT)
        provenance = response["_provenance"]
        self.assertEqual(provenance["source_pr"], "Generous-Corp/pulp#8811")
        self.assertEqual(provenance["cut_event_created_at"], "2026-09-25T04:12:51Z")
        self.assertEqual(provenance["ejecting_merge_group_run"], self.RUN_ID)
        got = guard.classify_pr_queue_state(response)
        self.assertEqual(got["class"], "ejected")
        self.assertEqual(got["reason"], "failed_checks")
        self.assertIs(got["new_head_since_removal"], False)
        self.assertEqual(got["head"], "e147f2d09972babcc9977e82a46470e17f9de538")
        # Control: with no attribution at all this is the refusal that shipped.
        allowed, message = guard.decide(got)
        self.assertFalse(allowed)
        self.assertIn("Push a fix first", message)

    def test_certified_infrastructure_allows_the_same_head_re_enqueue(self) -> None:
        self.declare(self.certifies())
        code, message, calls = self.run_guard([fixture(self.INCIDENT), *self.batch_reads()])
        self.assertEqual(code, 0, message)
        # A certified allow lifts a protective refusal, so it is audited.
        self.assertIn("queue-arm-guard: note:", message)
        self.assertIn("batch attributor certified", message)
        self.assertIn("Install ccache (macOS)", message)
        # The resolver picked the ejecting run, not the newer failed batch.
        self.assertIn(f"repos/Generous-Corp/pulp/actions/runs/{self.RUN_ID}/jobs", calls[2][1])
        argv = (self.root / "argv").read_text(encoding="utf-8")
        self.assertIn("--pr 8811", argv)
        self.assertIn(f"--run-id {self.RUN_ID}", argv)
        self.assertIn("--repo Generous-Corp/pulp", argv)

    def test_resolver_skips_runs_created_after_the_ejection(self) -> None:
        """36103973610 is newer but ran at 06:41, after the 04:12:51 removal."""
        listing = fixture("merge_group_failed_runs_listing.json")
        ids = [run["id"] for run in listing["workflow_runs"]]
        self.assertIn(36103973610, ids)  # control: the later run is in the listing
        self.assertIn(self.RUN_ID, ids)
        self.declare(self.certifies())
        _, _, calls = self.run_guard([fixture(self.INCIDENT), *self.batch_reads()])
        self.assertNotIn("36103973610", "".join(calls[2]))

    # -- what must still refuse -------------------------------------------

    def test_absence_of_a_test_failure_does_not_certify(self) -> None:
        """The exact signal Pulp's attributor emitted for this batch.

        `no ctest failure block (failure is not a test failure)` is true of a
        compile error too, which is the most common way a head breaks a batch.
        """
        self.declare({"run_id": self.RUN_ID, "verdict": "no_test_failure", "implicates_head": None})
        code, message, _ = self.run_guard([fixture(self.INCIDENT), *self.batch_reads()])
        self.assertEqual(code, 1)
        self.assertIn("did not certify this head", message)
        self.assertIn("Push a fix first", message)
        # The refusal now carries the batch evidence instead of nothing.
        self.assertIn("Install ccache (macOS)", message)
        self.assertIn(str(self.RUN_ID), message)

    def test_a_batch_that_failed_a_test_still_refuses(self) -> None:
        """Live capture of run 36103973610: a real ctest failure in the batch."""
        run = fixture("merge_group_run_real_test_failure.json")
        steps = [
            step["name"]
            for job in run["jobs"]["jobs"]
            if job["conclusion"] == "failure"
            for step in job["steps"]
            if step["conclusion"] == "failure"
        ]
        self.assertIn("Test (non-Windows)", steps)  # control: it really is a test failure
        self.declare({"run_id": self.RUN_ID, "implicates_head": True, "verdict": "this_pull_request"})
        code, message, _ = self.run_guard(
            [fixture(self.INCIDENT), *self.batch_reads("merge_group_run_real_test_failure.json")]
        )
        self.assertEqual(code, 1)
        self.assertIn("Test (non-Windows)", message)
        self.assertIn("Push a fix first", message)

    def test_merge_conflict_never_consults_an_attributor(self) -> None:
        self.declare(self.certifies())
        response = copy.deepcopy(fixture(self.INCIDENT))
        for node in response["data"]["repository"]["pullRequest"]["timelineItems"]["nodes"]:
            if node["__typename"] == "RemovedFromMergeQueueEvent":
                node["reason"] = "merge_conflict"
        code, message, calls = self.run_guard([response])
        self.assertEqual(code, 1)
        self.assertIn("ejected for merge_conflict", message)
        self.assertEqual(len(calls), 1)  # only the PR read; no batch reads at all
        self.assertFalse((self.root / "argv").exists())

    def test_no_declared_attributor_reads_nothing_extra(self) -> None:
        code, message, calls = self.run_guard([fixture(self.INCIDENT)])
        self.assertEqual(code, 1)
        self.assertIn("Push a fix first", message)
        self.assertEqual(len(calls), 1)

    def test_a_verdict_about_another_run_does_not_apply(self) -> None:
        self.declare(self.certifies(run_id=1))
        code, message, _ = self.run_guard([fixture(self.INCIDENT), *self.batch_reads()])
        self.assertEqual(code, 1)
        self.assertIn("ruled on run 1", message)

    def test_blaming_another_pull_request_allows_but_blaming_this_one_refuses(self) -> None:
        self.declare(self.certifies(verdict="other_pull_request", implicated_pr=8807))
        code, message, _ = self.run_guard([fixture(self.INCIDENT), *self.batch_reads()])
        self.assertEqual(code, 0, message)
        self.declare(self.certifies(verdict="other_pull_request", implicated_pr=8811))
        code, message, _ = self.run_guard([fixture(self.INCIDENT), *self.batch_reads()])
        self.assertEqual(code, 1)
        self.assertIn("named 8811", message)
        self.declare(self.certifies(verdict="other_pull_request"))
        code, message, _ = self.run_guard([fixture(self.INCIDENT), *self.batch_reads()])
        self.assertEqual(code, 1)
        self.assertIn("named None", message)

    def test_a_broken_attributor_refuses(self) -> None:
        for label, verdict, kwargs in (
            ("non-zero exit", self.certifies(), {"exit_code": 3}),
            ("not JSON", "not json at all", {}),
            ("not an object", "[1, 2]", {}),
        ):
            with self.subTest(label=label):
                self.declare(verdict, **kwargs)
                code, message, _ = self.run_guard([fixture(self.INCIDENT), *self.batch_reads()])
                self.assertEqual(code, 1)
                self.assertIn("Push a fix first", message)

    def test_a_crashed_attributor_s_stderr_tail_reaches_the_refusal(self) -> None:
        """A bare "exited 1" hid a FileNotFoundError for `ghapp` off the guard's PATH."""
        traceback = (
            "Traceback (most recent call last):\n"
            + "".join(f'  File "attribute.py", line {n}, in frame\n' for n in range(20))
            + "\x1b[31mFileNotFoundError: [Errno 2] No such file or directory: 'ghapp'\x1b[0m\n"
        )
        self.declare("", exit_code=1, stderr=traceback)
        code, message, _ = self.run_guard([fixture(self.INCIDENT), *self.batch_reads()])
        self.assertEqual(code, 1)
        self.assertIn("The attributor exited 1", message)
        self.assertIn("No such file or directory: 'ghapp'", message)
        # A tail, not the whole traceback, and no terminal escapes.
        self.assertNotIn("line 0,", message)
        self.assertNotIn("\x1b", message)

    def test_a_silent_failure_adds_no_stderr_clause(self) -> None:
        self.declare("", exit_code=1)
        _, message, _ = self.run_guard([fixture(self.INCIDENT), *self.batch_reads()])
        self.assertIn("The attributor exited 1", message)
        self.assertNotIn("Attributor stderr", message)

    def test_an_unresolvable_ejecting_batch_refuses(self) -> None:
        self.declare(self.certifies())
        empty = {"total_count": 0, "workflow_runs": []}
        code, message, _ = self.run_guard([fixture(self.INCIDENT), empty])
        self.assertEqual(code, 1)
        self.assertIn("cannot be identified", message)
        self.declare(self.certifies())
        code, message, _ = self.run_guard(
            [fixture(self.INCIDENT), guard.GuardError("HTTP 502 listing runs")]
        )
        self.assertEqual(code, 1)
        self.assertIn("HTTP 502", message)

    def test_a_run_with_no_failing_job_is_not_evidence(self) -> None:
        self.declare(self.certifies())
        jobs = copy.deepcopy(fixture("merge_group_run_real_infra_and_build.json")["jobs"])
        for job in jobs["jobs"]:
            job["conclusion"] = "success"
        code, message, _ = self.run_guard(
            [fixture(self.INCIDENT), fixture("merge_group_failed_runs_listing.json"), jobs]
        )
        self.assertEqual(code, 1)
        self.assertIn("no failing job", message)

    def test_an_attributor_from_another_repository_cannot_rule(self) -> None:
        self.declare(self.certifies())
        code, message, calls = self.run_guard([fixture(self.INCIDENT)], repo="someone/else")
        self.assertEqual(code, 1)
        self.assertIn("belongs to someone/else", message)
        self.assertEqual(len(calls), 1)

    def test_a_shell_string_command_is_rejected(self) -> None:
        self.declare(self.certifies(), command='"python3 attribute.py"')
        code, message, _ = self.run_guard([fixture(self.INCIDENT)])
        self.assertEqual(code, 1)
        self.assertIn("must be a non-empty list of strings", message)

    def test_an_ancestry_probe_finds_a_batch_this_pr_did_not_name(self) -> None:
        """A batch is named after one entry, so a mid-batch member needs ancestry.

        `compare/base...head` reports relative to the BASE, so a batch head built
        on this PR's head is `ahead` of it. Verified live on the incident:
        compare/e147f2d0...9b4c3892 is `ahead`, and the reverse is `behind`.
        """
        listing = copy.deepcopy(fixture("merge_group_failed_runs_listing.json"))
        for run in listing["workflow_runs"]:
            run["head_branch"] = run["head_branch"].replace("pr-8811-", "pr-9999-")
        self.declare(self.certifies())
        code, message, calls = self.run_guard(
            [
                fixture(self.INCIDENT),
                listing,
                {"status": "ahead"},
                fixture("merge_group_run_real_infra_and_build.json")["jobs"],
            ]
        )
        self.assertEqual(code, 0, message)
        self.assertIn("compare/e147f2d09972babcc9977e82a46470e17f9de538...", calls[2][1])
        self.assertIn("...9b4c3892f326382317340e020e7947c0890d5539", calls[2][1])

    def test_a_batch_that_does_not_contain_this_head_is_not_the_ejecting_batch(self) -> None:
        """`behind` and `diverged` both mean the batch never contained this head."""
        listing = copy.deepcopy(fixture("merge_group_failed_runs_listing.json"))
        for run in listing["workflow_runs"]:
            run["head_branch"] = run["head_branch"].replace("pr-8811-", "pr-9999-")
        for status in ("behind", "diverged"):
            with self.subTest(status=status):
                self.declare(self.certifies())
                code, message, _ = self.run_guard(
                    [fixture(self.INCIDENT), listing, *[{"status": status}] * 3]
                )
                self.assertEqual(code, 1)
                self.assertIn("cannot be identified", message)

    def test_the_named_batch_wins_before_any_ancestry_probe_is_spent(self) -> None:
        """A probe budget must not be able to hide a named match further down."""
        listing = copy.deepcopy(fixture("merge_group_failed_runs_listing.json"))
        runs = listing["workflow_runs"]
        named = next(run for run in runs if "pr-8811-" in run["head_branch"])
        others = [run for run in runs if run is not named]
        # Push the named run behind more decoys than the probe budget allows.
        for index, run in enumerate(others):
            run["created_at"] = "2026-09-25T04:1%d:00Z" % min(index, 2)
        listing["workflow_runs"] = others + [named]
        self.assertGreater(len(others), guard.MERGE_GROUP_ANCESTRY_PROBES)  # control
        self.declare(self.certifies())
        code, message, calls = self.run_guard(
            [
                fixture(self.INCIDENT),
                listing,
                fixture("merge_group_run_real_infra_and_build.json")["jobs"],
            ]
        )
        self.assertEqual(code, 0, message)
        # No compare call was needed at all.
        self.assertTrue(all("compare" not in "".join(call) for call in calls), calls)

    # -- the ejecting run is found by time, not by recency ------------------

    REMOVED_AT = "2026-09-25T04:12:51Z"

    def ejecting_run(self) -> dict[str, Any]:
        listing = fixture("merge_group_failed_runs_listing.json")
        return next(run for run in listing["workflow_runs"] if run["id"] == self.RUN_ID)

    @staticmethod
    def decoy(index: int, created_at: str) -> dict[str, Any]:
        return {
            "id": 40000000000 + index,
            "name": "Build and Test",
            "event": "merge_group",
            "conclusion": "failure",
            "head_branch": f"gh-readonly-queue/main/pr-{9100 + index}-{index:040x}",
            "head_sha": f"{index + 1:040x}",
            "created_at": created_at,
            "html_url": f"https://github.com/Generous-Corp/pulp/actions/runs/{40000000000 + index}",
        }

    def run_guard_against(self, pool: list[dict[str, Any]]) -> tuple[int, str, list[list[str]]]:
        """Run the guard against a fake that honours the listing's query string.

        The run listing is served like GitHub serves it: newest first, filtered
        by `created=A..B` (inclusive) when given, sliced by `per_page`/`page`.
        A fake that ignored `per_page` would hide exactly the recency bug.
        """
        calls: list[list[str]] = []
        jobs = fixture("merge_group_run_real_infra_and_build.json")["jobs"]

        def fake_gh(arguments: list[str]) -> Any:
            calls.append(arguments)
            joined = " ".join(arguments)
            if "graphql" in joined:
                return fixture(self.INCIDENT)
            if f"/actions/runs/{self.RUN_ID}/jobs" in joined:
                return jobs
            if "/compare/" in joined:
                return {"status": "behind"}
            if "/actions/runs?" in joined:
                query = dict(
                    part.split("=", 1) for part in arguments[1].split("?", 1)[1].split("&")
                )
                runs = sorted(pool, key=lambda run: run["created_at"], reverse=True)
                if "created" in query:
                    low, high = query["created"].split("..")
                    runs = [run for run in runs if low <= run["created_at"] <= high]
                size = int(query.get("per_page", 30))
                page = int(query.get("page", 1))
                return {
                    "total_count": len(runs),
                    "workflow_runs": runs[(page - 1) * size : page * size],
                }
            raise AssertionError(f"unexpected gh call {arguments}")

        stderr = io.StringIO()
        env = {"GH_REPO": "Generous-Corp/pulp", "PATH": os.environ.get("PATH", "/usr/bin:/bin")}
        with (
            mock.patch.dict(os.environ, env, clear=True),
            mock.patch.object(guard, "run_real_gh", side_effect=fake_gh),
            mock.patch.object(
                guard.PARSER, "current_repo_identity", return_value=("Generous-Corp", "pulp")
            ),
            contextlib.redirect_stderr(stderr),
        ):
            code = guard.main(["pr", "merge", "8811", "--auto"])
        return code, stderr.getvalue(), calls

    @staticmethod
    def listing_calls(calls: list[list[str]]) -> list[str]:
        return [call[1] for call in calls if len(call) > 1 and "/actions/runs?" in call[1]]

    def test_an_ejecting_run_behind_many_later_failures_is_still_found(self) -> None:
        """Guard run hours after the removal: the queue kept failing meanwhile.

        On a busy queue each workflow of each failed group is its own run, so
        more than a page of newer failures lands between the ejection and the
        re-enqueue. Pulp#9048 was refused this way about an hour after its
        ejection.
        """
        later = [self.decoy(i, "2026-09-25T09:%02d:00Z" % i) for i in range(40)]
        self.declare(self.certifies())
        code, message, calls = self.run_guard_against([*later, self.ejecting_run()])
        self.assertEqual(code, 0, message)
        self.assertIn("batch attributor certified", message)
        listings = self.listing_calls(calls)
        self.assertEqual(len(listings), 1)
        self.assertIn("created=2026-09-24T22:12:51Z..2026-09-25T04:17:51Z", listings[0])
        self.assertIn("event=merge_group&status=failure", listings[0])
        # The jobs read is unchanged: the resolved run, one large page.
        self.assertTrue(
            any(f"runs/{self.RUN_ID}/jobs?per_page=100" in call[1] for call in calls), calls
        )

    def test_a_run_named_for_this_pr_is_found_on_a_later_page(self) -> None:
        """More in-window failures than one page: the named run is on page 2."""
        busier = [
            self.decoy(i, "2026-09-25T04:%02d:%02dZ" % (7 + i // 60, i % 60)) for i in range(150)
        ]
        self.assertTrue(all(run["created_at"] <= self.REMOVED_AT for run in busier))  # control
        self.declare(self.certifies())
        code, message, calls = self.run_guard_against([*busier, self.ejecting_run()])
        self.assertEqual(code, 0, message)
        listings = self.listing_calls(calls)
        self.assertEqual(len(listings), 2)
        self.assertIn("&page=2", listings[1])
        # Found by its queue branch name, so no ancestry probe was spent.
        self.assertTrue(all("compare" not in "".join(call) for call in calls), calls)

    def test_nothing_in_the_window_refuses(self) -> None:
        """A run from before the window cannot be the ejector, named or not."""
        stale = copy.deepcopy(self.ejecting_run())
        stale["created_at"] = "2026-09-24T20:00:00Z"
        later = [self.decoy(i, "2026-09-25T09:%02d:00Z" % i) for i in range(5)]
        self.declare(self.certifies())
        code, message, calls = self.run_guard_against([stale, *later])
        self.assertEqual(code, 1)
        self.assertIn("cannot be identified", message)
        self.assertIn("created 2026-09-24T22:12:51Z..2026-09-25T04:17:51Z", message)
        self.assertIn("Push a fix first", message)
        self.assertFalse((self.root / "argv").exists())

    def test_the_listing_read_cap_is_respected(self) -> None:
        """A window busier than the cap refuses rather than reading forever."""
        cap = guard.MERGE_GROUP_RUN_MAX_PAGES * guard.MERGE_GROUP_RUN_PAGE
        # Every decoy is inside the window and newer than the ejecting run.
        crowd = [
            self.decoy(i, "2026-09-25T04:%02d:%02dZ" % (7 + i % 5, i % 60))
            for i in range(cap + 50)
        ]
        self.declare(self.certifies())
        code, message, calls = self.run_guard_against([*crowd, self.ejecting_run()])
        self.assertEqual(code, 1)
        self.assertIn("read cap reached", message)
        self.assertEqual(len(self.listing_calls(calls)), guard.MERGE_GROUP_RUN_MAX_PAGES)
        probes = [call for call in calls if "compare" in "".join(call)]
        self.assertEqual(len(probes), guard.MERGE_GROUP_ANCESTRY_PROBES)
        self.assertFalse((self.root / "argv").exists())

    def test_jobs_are_read_without_gh_paginate(self) -> None:
        """`gh api --paginate` concatenates one object per page, which is not JSON."""
        self.declare(self.certifies())
        _, _, calls = self.run_guard([fixture(self.INCIDENT), *self.batch_reads()])
        jobs_call = calls[2]
        self.assertNotIn("--paginate", jobs_call)
        self.assertIn("per_page=100", jobs_call[1])

    def test_a_repo_relative_program_runs_from_the_repo_root(self) -> None:
        """A repo-relative `command` is supported, run with cwd at the repo root.

        Characterization, not a guard: POSIX resolves this natively because
        CPython chdirs to `cwd` before `execv`, so no code in the guard
        implements it and breaking the guard cannot make this test fail. It is
        here to pin the supported shape of `command`.
        """
        runner = self.root / "bin" / "attribute.sh"
        runner.parent.mkdir(parents=True, exist_ok=True)
        runner.write_text(
            "#!/bin/sh\nprintf '%s' '" + json.dumps(self.certifies()) + "'\n", encoding="utf-8"
        )
        runner.chmod(runner.stat().st_mode | stat.S_IXUSR)
        (self.root / ".shipyard").mkdir(parents=True, exist_ok=True)
        (self.root / ".shipyard" / "config.toml").write_text(
            '[queue.attribution]\ncommand = ["bin/attribute.sh"]\n', encoding="utf-8"
        )
        # Run from a subdirectory so the parent cwd is not the repo root.
        nested = self.root / "deep" / "deeper"
        nested.mkdir(parents=True, exist_ok=True)
        os.chdir(nested)
        code, message, _ = self.run_guard([fixture(self.INCIDENT), *self.batch_reads()])
        self.assertEqual(code, 0, message)

    def test_no_fixture_reaches_an_attributor_when_none_is_declared(self) -> None:
        """The compatibility contract: opting out costs nothing and changes nothing.

        Installing this guard must not alter behaviour for a repository with no
        `[queue.attribution]`, because the guards directory is shared by every
        Shipyard binary on the host.
        """
        asked = []
        with mock.patch.object(
            guard, "resolve_ejecting_batch", side_effect=AssertionError("resolved a batch")
        ), mock.patch.object(guard, "attributor_command", wraps=guard.attributor_command) as spy:
            for path in sorted(FIXTURES.glob("pr_*.json")):
                body = json.loads(path.read_text(encoding="utf-8"))
                classified = guard.classify_pr_queue_state(body)
                verdict = guard.attribute_ejecting_batch(classified, "Generous-Corp/pulp")
                self.assertIsNone(verdict, path.name)
                # An uncertified/None verdict must leave the decision untouched.
                self.assertEqual(
                    guard.decide(classified), guard.decide(classified, verdict), path.name
                )
                asked.append(path.name)
            # Control: the corpus really was walked, and same-head failed_checks
            # cases (the only ones that could consult an attributor) were in it.
            self.assertGreaterEqual(len(asked), 10)
            self.assertIn(self.INCIDENT, asked)
            self.assertGreaterEqual(spy.call_count, 1)

    def test_certification_never_names_the_override(self) -> None:
        for verdict in (self.certifies(), {"run_id": self.RUN_ID, "implicates_head": None}):
            with self.subTest(verdict=verdict):
                self.declare(verdict)
                _, message, _ = self.run_guard([fixture(self.INCIDENT), *self.batch_reads()])
                for name in ("GHAPP_ALLOW_QUEUE_REARM", "SHIPYARD_INTERNAL_QUEUE_MUTATION"):
                    self.assertNotIn(name, message)


    def test_installed_guard_certifies_through_a_real_subprocess(self) -> None:
        """The whole chain as ghapp runs it: installed names, real gh, real attributor."""
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            guards = root / "guards"
            guards.mkdir()
            shutil.copy(SCRIPT, guards / "queue-arm-guard")
            shutil.copy(
                SCRIPT.with_name("ghapp_queue_removal_guard.py"), guards / "queue-removal-guard"
            )
            # A repo checkout that declares an attributor.
            repo = root / "repo"
            (repo / ".shipyard").mkdir(parents=True)
            attributor = repo / "attribute.py"
            attributor.write_text(
                "import json, sys\n"
                "args = dict(zip(sys.argv[1::2], sys.argv[2::2]))\n"
                "print(json.dumps({'run_id': int(args['--run-id']),\n"
                "                  'implicates_head': False,\n"
                "                  'verdict': 'infrastructure',\n"
                "                  'evidence': 'macos failed before any content built',\n"
                "                  'saw': args['--repo'] + '#' + args['--pr']}))\n",
                encoding="utf-8",
            )
            (repo / ".shipyard" / "config.toml").write_text(
                f'[queue.attribution]\ncommand = ["{sys.executable}", "attribute.py"]\n',
                encoding="utf-8",
            )
            # A gh that answers the three reads in order: PR, run listing, jobs.
            answers = guards / "answers"
            answers.mkdir()
            for index, name in enumerate(
                (
                    self.INCIDENT,
                    "merge_group_failed_runs_listing.json",
                    "merge_group_run_real_infra_and_build.json",
                )
            ):
                body = fixture(name)
                if name.startswith("merge_group_run_"):
                    body = body["jobs"]
                (answers / f"{index}.json").write_text(json.dumps(body), encoding="utf-8")
            # Then the review reads: the pull request, its comments (one
            # approval of the head, shaped like Shipyard#784's), its reviews.
            head = "e147f2d09972babcc9977e82a46470e17f9de538"
            approval = copy.deepcopy(next(
                c for c in review_fixture("shipyard-784-comments.json")[0] if c["id"] == 6127196414
            ))
            approval["body"] = f"Delta review.\n\nverdict: APPROVE head:{head}"
            for index, body in enumerate(
                (
                    {"number": 8811, "base": {"ref": "main"}, "head": {"sha": head},
                     "user": {"login": "shipyard-local[bot]"}},
                    [[approval]],
                    [[]],
                ),
                start=3,
            ):
                (answers / f"{index}.json").write_text(json.dumps(body), encoding="utf-8")
            fake_gh = guards / "gh"
            fake_gh.write_text(
                "#!/bin/sh\n"
                f"printf '%s\\n' \"$*\" >> '{guards / 'calls'}'\n"
                "case \"$*\" in *contents/.shipyard/config.toml*)\n"
                "  echo 'gh: Not Found (HTTP 404)' >&2; exit 1;; esac\n"
                f"n=$(cat '{guards / 'n'}' 2>/dev/null || echo 0)\n"
                f"echo $((n+1)) > '{guards / 'n'}'\n"
                f"cat '{answers}'/$n.json\n",
                encoding="utf-8",
            )
            for executable in (fake_gh, guards / "queue-arm-guard"):
                executable.chmod(executable.stat().st_mode | stat.S_IXUSR)
            result = subprocess.run(
                [sys.executable, str(guards / "queue-arm-guard"), "pr", "merge", "8811", "--auto",
                 "--match-head-commit", head],
                env={
                    "PATH": "/usr/bin:/bin",
                    "GHAPP_REAL_GH": str(fake_gh),
                    "GH_REPO": "Generous-Corp/pulp",
                    "SHIPYARD_REVIEW_BEFORE_ARM": "1",
                },
                cwd=str(repo),
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("batch attributor certified", result.stderr)
            self.assertIn("Install ccache (macOS)", result.stderr)
            calls = (guards / "calls").read_text(encoding="utf-8")
            self.assertIn("number=8811", calls)
            self.assertIn("event=merge_group&status=failure", calls)
            self.assertIn(f"runs/{self.RUN_ID}/jobs", calls)
            self.assertIn("repos/Generous-Corp/pulp/issues/8811/comments", calls)



# ---------------------------------------------------------------------------
# Environment re-enqueue: the Python twin of src/environment_requeue.rs.
# ---------------------------------------------------------------------------

REPO = "Generous-Corp/pulp"


def job_fixture(name: str) -> dict[str, Any]:
    return fixture(f"job_logs/{name}")


def failing_steps(job: dict[str, Any]) -> list[tuple[str, str]]:
    return [
        (step["name"], step["started_at"])
        for step in job["job"]["steps"]
        if step["conclusion"] == "failure"
    ]


class FakeApi:
    """Answers the environment reader's reads from real captures."""

    def __init__(self) -> None:
        checks = fixture("merge_group_required_checks_real.json")
        self.json: dict[str, Any] = {
            f"repos/{REPO}/rules/branches/main": checks["rules_branches_main"],
            f"repos/{REPO}/branches/main/protection/required_status_checks": checks[
                "required_status_checks"
            ],
        }
        for sha, commit in checks["commits"].items():
            self.json[f"repos/{REPO}/commits/{sha}/check-runs?per_page=100"] = commit["check_runs"]
            self.json[f"repos/{REPO}/commits/{sha}/status"] = commit["status"]
        for pr in (8678, 8911, 8933):
            self.json[f"repos/{REPO}/pulls/{pr}"] = {"base": {"ref": "main"}}
        self.text: dict[str, Any] = {}
        for name in (
            "job_real_pip_relay_403.json",
            "job_real_cargo_dns_in_build.json",
            "job_real_8933_test_failure.json",
        ):
            job = job_fixture(name)
            job_id = job["job"]["id"]
            self.json[f"repos/{REPO}/actions/jobs/{job_id}"] = job["job"]
            self.text[f"repos/{REPO}/actions/jobs/{job_id}/logs"] = job["log"]

    def api_json(self, arguments: list[str]) -> Any:
        value = self.json.get(arguments[1])
        if value is None:
            raise guard.GuardError(f"unexpected call {arguments[1]}")
        if isinstance(value, Exception):
            raise value
        return value

    def api_text(self, arguments: list[str]) -> str:
        value = self.text.get(arguments[1])
        if value is None:
            raise guard.GuardError(f"unexpected call {arguments[1]}")
        if isinstance(value, Exception):
            raise value
        return value


def assess(api: FakeApi, name: str, opted_in: bool = True) -> dict[str, Any] | None:
    classification = guard.classify_pr_queue_state(fixture(name))
    return guard.assess_environment_requeue(
        classification, REPO, opted_in, api_json=api.api_json, api_text=api.api_text
    )


class EnvironmentSignaturesAgreeWithSharedCorpus(unittest.TestCase):
    def test_every_job_log_matches_the_shared_expectations(self) -> None:
        expected = job_fixture("expected_environment_signatures.json")
        checked = 0
        for name, steps in expected.items():
            if name.startswith("_"):
                continue
            job = job_fixture(name)
            failing = failing_steps(job)
            self.assertEqual(len(failing), len(steps), name)
            for step, started in failing:
                with self.subTest(fixture=name, step=step):
                    reading = guard.read_failing_step(job["log"], started)
                    self.assertEqual(reading["reading"], steps[step]["reading"])
                    self.assertEqual(reading.get("signature"), steps[step]["signature"])
                    checked += 1
        self.assertEqual(checked, 6)

    def test_signature_sets_match_the_rust_module(self) -> None:
        rust = (FIXTURES.parent.parent.parent / "src" / "environment_requeue.rs").read_text()
        for signature in guard.ENVIRONMENT_SIGNATURES:
            self.assertIn(f'"{signature}"', rust)
        self.assertIn(
            f"FAILURE_PROXIMITY_LINES: usize = {guard.FAILURE_PROXIMITY_LINES};", rust
        )
        for signature in guard.INTERRUPTION_SIGNATURES:
            self.assertIn(f'"{signature}"', rust)
        self.assertIn(
            f"STARVATION_MIN_WAIT_MINUTES: i64 = {guard.STARVATION_MIN_WAIT_MINUTES};", rust
        )
        self.assertIn(f"INTERRUPTION_REARM_LIMIT: u32 = {guard.INTERRUPTION_REARM_LIMIT};", rust)
        for text in guard.SUPERSEDED_ANNOTATIONS:
            self.assertIn(f'"{text}"', rust)

    def test_a_signature_in_the_script_echo_is_not_output(self) -> None:
        log = job_fixture("job_real_8933_test_failure.json")["log"].splitlines()
        segment = [log[0]]
        for line in log[1:]:
            if "##[group]Run " in line:
                break
            segment.append(line)
        self.assertIn("##[group]Run set -euo pipefail", segment[0])
        # Control: the signature IS in the text handed over, inside the echo.
        self.assertTrue(any("Tunnel connection failed" in line for line in segment))
        synthetic = "\n".join(segment) + (
            "\n2026-09-29T06:33:30.0000000Z ##[error]Process completed with exit code 1.\n"
        )
        self.assertEqual(
            guard.read_failing_step(synthetic, segment[0][:20])["reading"], "no_signature"
        )
        # The property itself, independent of the proximity window: no echoed
        # script line is read as output.
        output = guard.failing_step_output(synthetic, segment[0][:20])
        assert output is not None
        self.assertFalse(any("Tunnel connection failed" in line for line in output))
        self.assertFalse(any(line.startswith("set -euo pipefail") for line in output))

    def test_a_step_is_read_from_its_own_start_not_an_earlier_error(self) -> None:
        # Real log: Chrome fails with curl (56) and a later continue-on-error
        # step errors with exit 2. Read at the later step's start, the earlier
        # step's signature must not be the answer.
        job = job_fixture("job_real_chrome_proxy_connect.json")
        later = next(
            step for step in job["job"]["steps"] if step["name"].startswith("Observe ctest non-runs")
        )
        reading = guard.read_failing_step(job["log"], later["started_at"])
        self.assertEqual(reading["reading"], "no_signature")

    def test_a_signature_far_from_the_failure_explains_nothing(self) -> None:
        def build(gap: int) -> str:
            lines = [
                "2026-09-29T00:00:00.0000000Z ##[group]Run make",
                "2026-09-29T00:00:00.1000000Z make",
                "2026-09-29T00:00:00.2000000Z ##[endgroup]",
                "2026-09-29T00:00:01.0000000Z warning: Could not resolve host: index.crates.io",
            ]
            lines += ["2026-09-29T00:00:01.0000000Z [12/40] Compiling foo.cpp"] * gap
            lines += [
                "2026-09-29T00:00:01.0000000Z foo.cpp:1:1: error: expected ';'",
                "2026-09-29T00:00:01.0000000Z ##[error]Process completed with exit code 1.",
            ]
            return "\n".join(lines) + "\n"

        edge = guard.FAILURE_PROXIMITY_LINES - 3
        self.assertEqual(guard.read_failing_step(build(edge), "2026-09-29T00:00:00Z")["reading"],
                         "environment")
        self.assertEqual(
            guard.read_failing_step(build(edge + 1), "2026-09-29T00:00:00Z")["reading"],
            "no_signature",
        )


GROUP = "c0ffee0000000000000000000000000000000001"

UPLOAD_STALL_LOG = (
    "2026-10-09T23:10:00.0000000Z ##[group]Run cargo test\n"
    "2026-10-09T23:10:00.0000000Z cargo test\n"
    "2026-10-09T23:10:00.0000000Z ##[endgroup]\n"
    "2026-10-09T23:30:00.0000000Z test result: ok. 22592 passed; 0 failed\n"
    "2026-10-09T23:30:01.0000000Z ##[group]Run actions/upload-artifact@v4\n"
    "2026-10-09T23:30:01.0000000Z with:\n"
    "2026-10-09T23:30:01.0000000Z ##[endgroup]\n"
    "2026-10-09T23:30:02.0000000Z Uploading 812 files\n"
    "2026-10-09T23:37:00.0000000Z ##[error]Upload progress stalled.\n"
)


def assess_interrupted(conclusion: str, job: dict[str, Any], log: str, ejections: int,
                       annotations: list[dict[str, Any]] | None = None) -> dict[str, Any]:
    """The twin of the Rust ``interrupted_group``: one required macos check on job 7."""
    annotations = annotations or []
    api = FakeApi()
    api.json[f"repos/{REPO}/branches/main/protection/required_status_checks"] = {"contexts": ["macos"]}
    api.json[f"repos/{REPO}/commits/{GROUP}/check-runs?per_page=100"] = {
        "total_count": 1,
        "check_runs": [{"id": 7, "name": "macos", "status": "completed",
                        "conclusion": conclusion, "app": {"slug": "github-actions"}}],
    }
    api.json[f"repos/{REPO}/commits/{GROUP}/status"] = {"statuses": []}
    api.json[f"repos/{REPO}/actions/jobs/7"] = job
    api.json[f"repos/{REPO}/check-runs/7/annotations"] = annotations
    api.text[f"repos/{REPO}/actions/jobs/7/logs"] = log
    classification = guard.classify_pr_queue_state(
        fixture("pr_real_8678_first_environment_ejection.json")
    )
    classification["ejections_of_current_head"] = ejections
    classification["last_ejection"] = {**classification["last_ejection"], "merge_group_commit": GROUP}
    verdict = guard.assess_environment_requeue(
        classification, REPO, True, api_json=api.api_json, api_text=api.api_text
    )
    assert verdict is not None
    return verdict


def queued_job(runner: str, completed: str | None) -> dict[str, Any]:
    return {"id": 7, "status": "completed", "conclusion": "cancelled",
            "created_at": "2026-10-08T10:00:00Z", "completed_at": completed,
            "runner_name": runner, "steps": []}


def upload_job(test_conclusion: str) -> dict[str, Any]:
    return {"id": 7, "status": "completed", "conclusion": "failure",
            "runner_name": "studio-pulp-gate-01",
            "steps": [
                {"name": "Test", "conclusion": test_conclusion, "started_at": "2026-10-09T23:10:00Z"},
                {"name": "Upload test artifacts", "conclusion": "failure",
                 "started_at": "2026-10-09T23:30:01Z"},
            ]}


class InterruptionRequeue(unittest.TestCase):
    """Same cases as the Rust module's interruption tests; the two must agree."""

    def test_a_starved_required_job_is_an_interruption_bounded_at_two(self) -> None:
        job = queued_job("", "2026-10-08T10:16:00Z")
        verdict = assess_interrupted("cancelled", job, "", 1)
        self.assertTrue(verdict["allowed"], verdict["reason"])
        self.assertEqual(verdict["class"], "interruption")
        self.assertEqual(verdict["evidence"][0]["signature"], "starved")
        self.assertTrue(assess_interrupted("cancelled", job, "", 2)["allowed"])
        third = assess_interrupted("cancelled", job, "", 3)
        self.assertFalse(third["allowed"])
        self.assertIn("3 times", third["reason"])

    def test_a_cancel_that_had_a_runner_or_never_waited_is_not_starvation(self) -> None:
        for runner, completed, why in (
            ("studio-pulp-gate-01", "2026-10-08T10:16:00Z", "a runner took it"),
            ("", "2026-10-08T10:02:00Z", "superseded after 2 minutes"),
            ("", None, "no completion time"),
        ):
            with self.subTest(why=why):
                verdict = assess_interrupted("cancelled", queued_job(runner, completed), "", 1)
                self.assertFalse(verdict["allowed"], verdict["reason"])
                self.assertEqual(verdict["evidence"][0]["reading"], "no_signature")

    def test_a_concurrency_supersede_is_not_starvation(self) -> None:
        # Run 37891885987's macos job: no runner, 16.5 min queued, superseded.
        job = queued_job("", "2026-10-08T10:16:30Z")
        annotations = [{"annotation_level": "failure", "message":
                        "Canceling since a higher priority waiting request for "
                        "build-refs/pull/9957/merge exists"}]
        verdict = assess_interrupted("cancelled", job, "", 1, annotations)
        self.assertFalse(verdict["allowed"], verdict["reason"])
        self.assertEqual(verdict["evidence"][0]["reading"], "no_signature")
        # Control: the same job without the annotation is starved.
        self.assertTrue(assess_interrupted("cancelled", job, "", 1, [])["allowed"])
        # Positive control: annotations present, none of them a supersede.
        not_acquired = [{"annotation_level": "failure", "message":
                         "The job was not acquired by Runner of type self-hosted even after "
                         "multiple attempts"}]
        verdict = assess_interrupted("cancelled", job, "", 1, not_acquired)
        self.assertTrue(verdict["allowed"], verdict["reason"])
        self.assertEqual(verdict["evidence"][0]["signature"], "starved")

    def test_an_upload_stall_after_a_green_test_step_is_an_interruption(self) -> None:
        verdict = assess_interrupted("failure", upload_job("success"), UPLOAD_STALL_LOG, 1)
        self.assertTrue(verdict["allowed"], verdict["reason"])
        self.assertEqual(verdict["class"], "interruption")
        self.assertEqual([step["step"] for step in verdict["evidence"]], ["Upload test artifacts"])

    def test_a_recovered_stall_earlier_in_the_step_does_not_explain_its_failure(self) -> None:
        log = UPLOAD_STALL_LOG.replace(
            "2026-10-09T23:37:00.0000000Z ##[error]Upload progress stalled.\n",
            "2026-10-09T23:31:00.0000000Z Upload progress stalled.\n"
            "2026-10-09T23:32:00.0000000Z Retrying upload\n"
            "2026-10-09T23:36:00.0000000Z Finalizing artifact\n"
            "2026-10-09T23:37:00.0000000Z ##[error]Artifact storage quota has been hit.\n",
        )
        verdict = assess_interrupted("failure", upload_job("success"), log, 1)
        self.assertFalse(verdict["allowed"], verdict["reason"])
        self.assertEqual(verdict["evidence"][0]["reading"], "no_signature")

    def test_a_real_red_before_a_stalled_upload_stays_refused(self) -> None:
        log = UPLOAD_STALL_LOG.replace(
            "test result: ok. 22592 passed; 0 failed",
            "test result: FAILED. 22591 passed; 1 failed\n"
            "2026-10-09T23:30:00.5000000Z ##[error]Process completed with exit code 101.",
        )
        verdict = assess_interrupted("failure", upload_job("failure"), log, 1)
        self.assertFalse(verdict["allowed"], verdict["reason"])
        self.assertIn("Test", verdict["reason"])


class EnvironmentRequeueWorkedExamples(unittest.TestCase):
    def test_pip_relay_ejection_of_8678_allows_one_re_enqueue(self) -> None:
        verdict = assess(FakeApi(), "pr_real_8678_first_environment_ejection.json")
        assert verdict is not None
        self.assertTrue(verdict["allowed"], verdict["reason"])
        self.assertEqual(
            [(step["check"], step["step"]) for step in verdict["evidence"]],
            [("macos", "Install visual-analysis Python dependencies")],
        )
        classification = guard.classify_pr_queue_state(
            fixture("pr_real_8678_first_environment_ejection.json")
        )
        allowed, message = guard.decide(classification, None, verdict)
        self.assertTrue(allowed)
        self.assertIn("this re-enqueue is allowed", message)
        self.assertIn("Tunnel connection failed", message)
        # Control: the same classification without the verdict is refused.
        self.assertFalse(guard.decide(classification)[0])

    def test_cargo_dns_ejection_of_8911_allows_one_re_enqueue(self) -> None:
        verdict = assess(FakeApi(), "pr_real_8911_environment_ejection.json")
        assert verdict is not None
        self.assertTrue(verdict["allowed"], verdict["reason"])
        self.assertEqual(verdict["evidence"][0]["signature"], "Could not resolve host")

    def test_test_failure_ejection_of_8933_stays_refused(self) -> None:
        verdict = assess(FakeApi(), "pr_real_8933_test_failure_ejection.json")
        assert verdict is not None
        self.assertFalse(verdict["allowed"])
        self.assertIn("macos / Test (non-Windows)", verdict["reason"])
        classification = guard.classify_pr_queue_state(
            fixture("pr_real_8933_test_failure_ejection.json")
        )
        allowed, message = guard.decide(classification, None, verdict)
        self.assertFalse(allowed)
        self.assertIn("Environment re-enqueue refused", message)

    def test_a_second_ejection_of_the_same_head_is_refused(self) -> None:
        verdict = assess(FakeApi(), "pr_real_8678_second_ejection_same_head.json")
        assert verdict is not None
        self.assertFalse(verdict["allowed"], verdict["reason"])

    def test_a_network_failure_allows_only_the_first_ejection(self) -> None:
        classification = guard.classify_pr_queue_state(
            fixture("pr_real_8678_first_environment_ejection.json")
        )
        classification["ejections_of_current_head"] = 2
        api = FakeApi()
        verdict = guard.assess_environment_requeue(
            classification, REPO, True, api_json=api.api_json, api_text=api.api_text
        )
        assert verdict is not None
        self.assertFalse(verdict["allowed"])
        self.assertIn("2 times", verdict["reason"])
        self.assertEqual(verdict["evidence"][0]["reading"], "environment")

    def test_not_opted_in_is_refused_without_reading(self) -> None:
        verdict = assess(FakeApi(), "pr_real_8678_first_environment_ejection.json", False)
        assert verdict is not None
        self.assertFalse(verdict["allowed"])
        self.assertIn("environment_requeue", verdict["reason"])

    def test_an_unreadable_log_refuses(self) -> None:
        api = FakeApi()
        api.text[f"repos/{REPO}/actions/jobs/107036733221/logs"] = guard.GuardError("HTTP 404")
        verdict = assess(api, "pr_real_8678_first_environment_ejection.json")
        assert verdict is not None
        self.assertFalse(verdict["allowed"])
        self.assertIn("could not be read", verdict["reason"])

    def test_a_failed_job_with_no_failing_step_refuses(self) -> None:
        api = FakeApi()
        job = dict(api.json[f"repos/{REPO}/actions/jobs/107036733221"])
        job["steps"] = [dict(step, conclusion="success") for step in job["steps"]]
        api.json[f"repos/{REPO}/actions/jobs/107036733221"] = job
        verdict = assess(api, "pr_real_8678_first_environment_ejection.json")
        assert verdict is not None
        self.assertFalse(verdict["allowed"])
        self.assertIn("no failing step recorded", verdict["reason"])

    def test_only_same_head_failed_checks_ejections_are_assessed(self) -> None:
        for name in ("pr_queued.json", "pr_never_armed.json", "pr_ejected_new_head.json"):
            self.assertIsNone(assess(FakeApi(), name), name)

    def test_opt_in_is_read_from_the_repository_config(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            config = pathlib.Path(root) / ".shipyard" / "config.toml"
            config.parent.mkdir()
            config.write_text("[queue.environment_requeue]\nenabled = true\n", encoding="utf-8")
            nested = pathlib.Path(root) / "a" / "b"
            nested.mkdir(parents=True)
            self.assertTrue(guard.environment_requeue_enabled(nested))
            config.write_text("[queue.environment_requeue]\nenabled = \"yes\"\n", encoding="utf-8")
            self.assertFalse(guard.environment_requeue_enabled(nested))
            config.write_text("[queue]\n", encoding="utf-8")
            self.assertFalse(guard.environment_requeue_enabled(nested))


if __name__ == "__main__":
    unittest.main()


REMOVAL = SCRIPT.with_name("ghapp_queue_removal_guard.py")


def opened(graphql_response: dict[str, Any]) -> dict[str, Any]:
    """A merged pull request's captured response, as it read while open."""
    response = copy.deepcopy(graphql_response)
    pr = response["data"]["repository"]["pullRequest"]
    pr["state"] = "OPEN"
    pr["timelineItems"]["nodes"] = [
        node for node in pr["timelineItems"]["nodes"] if node.get("__typename") != "MergedEvent"
    ]
    return response


HEAD_784 = "fd0e94e25f447578256f3aae97cc0ab6103d5f0c"
HOLD_784 = "cb5c1f796dac6851062af69def634aade161fb1f"
APPROVE_784_COMMENT = 6127196414


def comment_like(template: dict[str, Any], **change: Any) -> dict[str, Any]:
    """A copy of a captured comment with some fields changed."""
    value = copy.deepcopy(template)
    value.update(change)
    return value


# GitHub's `compare/<x>...<y>` answered from a real git repository, for the
# fake gh: the merge base, ahead/behind status, the commits with parents, and
# each changed file's status, blob sha and whether it has a text patch.
GIT_COMPARE = r"""
import subprocess as _sp
def _git(repo, *args):
    return _sp.run(["git", *args], cwd=repo, capture_output=True, text=True, check=True).stdout.strip()
def git_compare(repo, argv):
    spec = next(part for part in argv.split() if "/compare/" in part).split("/compare/", 1)[1]
    x, y = spec.split("...", 1)
    base = _git(repo, "merge-base", x, y)
    ahead = int(_git(repo, "rev-list", "--count", f"{base}..{y}"))
    behind = int(_git(repo, "rev-list", "--count", f"{base}..{x}"))
    status = ("identical" if not ahead and not behind else "ahead" if not behind
              else "behind" if not ahead else "diverged")
    commits = []
    for line in _git(repo, "rev-list", "--reverse", "--parents", f"{base}..{y}").splitlines():
        sha, *parents = line.split()
        commits.append({"sha": sha, "parents": [{"sha": p} for p in parents]})
    binary = {line.split("\t")[2] for line in _git(repo, "diff", "--numstat", base, y).splitlines()
              if line.startswith("-\t-\t")}
    files = []
    for line in _git(repo, "diff", "--raw", "--no-abbrev", "--no-renames", base, y).splitlines():
        meta, name = line.split("\t", 1)
        _, _, old, new, letter = meta.split()
        entry = {"filename": name, "status": {"A": "added", "D": "removed"}.get(letter, "modified"),
                 "sha": new if letter != "D" else old}
        if name not in binary:
            entry["patch"] = "@@"
        files.append(entry)
    return {"status": status, "ahead_by": ahead, "behind_by": behind, "total_commits": ahead,
            "merge_base_commit": {"sha": base}, "commits": commits, "files": files}
def git_diff(repo, argv):
    spec = next(part for part in argv.split() if "/compare/" in part).split("/compare/", 1)[1]
    x, y = spec.split("...", 1)
    base = _git(repo, "merge-base", x, y)
    return _sp.run(["git", "diff", "--no-renames", "--full-index", base, y], cwd=repo,
                   capture_output=True, text=True, check=True).stdout
def git_blob(repo, argv):
    import base64 as _b64
    sha = next(part for part in argv.split() if "/git/blobs/" in part).rsplit("/", 1)[1]
    data = _sp.run(["git", "cat-file", "blob", sha], cwd=repo, capture_output=True, check=True).stdout
    return {"sha": sha, "encoding": "base64", "content": _b64.b64encode(data).decode()}
def git_trees(repo, argv):
    import re as _re
    found = {}
    for alias, expression in _re.findall(r'(d\d+):object\(expression:("(?:[^"\\]|\\.)*")\)', argv):
        commit, _, directory = json.loads(expression).partition(":")
        listing = _sp.run(["git", "ls-tree", f"{commit}:{directory}" if directory else commit],
                          cwd=repo, capture_output=True, text=True)
        if listing.returncode != 0:
            found[alias] = None
            continue
        entries = []
        for line in listing.stdout.splitlines():
            meta, name = line.split("\t", 1)
            entries.append({"name": name, "mode": int(meta.split()[0], 8)})
        found[alias] = {"entries": entries}
    return {"data": {"repository": found}}
"""


class ReviewBeforeArm(unittest.TestCase):
    """A landing needs a verdict naming the exact head, through the installed guard.

    Each case runs the guard as ghapp does: installed names, a real
    subprocess, and a fake native gh answering from live captures. Its
    answers are matched in order by a substring of the argv; a paginated
    answer serves every page only when the request carries ``--paginate``.
    """

    def setUp(self) -> None:
        self.root = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root, True)
        self.guards = self.root / "guards"
        self.guards.mkdir()
        shutil.copy(SCRIPT, self.guards / "queue-arm-guard")
        shutil.copy(REMOVAL, self.guards / "queue-removal-guard")

    def install(self, answers: list[tuple[str, dict[str, Any]]]) -> pathlib.Path:
        table = self.root / "answers.json"
        table.write_text(json.dumps(answers), encoding="utf-8")
        fake = self.root / "gh"
        fake.write_text(
            f"#!{sys.executable}\n"
            "import json, sys\n"
            + GIT_COMPARE +
            f"answers = json.load(open({str(table)!r}))\n"
            "argv = ' '.join(sys.argv[1:])\n"
            f"open({str(self.root / 'calls')!r}, 'a').write(argv + '\\n')\n"
            "for needle, answer in answers:\n"
            "    if needle not in argv:\n"
            "        continue\n"
            "    if 'git_tree' in answer:\n"
            "        sys.stdout.write(json.dumps(git_trees(answer['git_tree'], argv))); sys.exit(0)\n"
            "    if 'git_blob' in answer:\n"
            "        sys.stdout.write(json.dumps(git_blob(answer['git_blob'], argv))); sys.exit(0)\n"
            "    if 'git' in answer and 'vnd.github.diff' in argv:\n"
            "        sys.stdout.write(git_diff(answer['git'], argv)); sys.exit(0)\n"
            "    if 'git' in answer:\n"
            "        sys.stdout.write(json.dumps(git_compare(answer['git'], argv))); sys.exit(0)\n"
            "    if 'error' in answer:\n"
            "        sys.stderr.write(answer['error'] + '\\n'); sys.exit(1)\n"
            "    if 'pages' in answer:\n"
            "        pages = answer['pages'] if '--paginate' in sys.argv else answer['pages'][:1]\n"
            "        sys.stdout.write(json.dumps(pages if '--slurp' in sys.argv else pages[0])); sys.exit(0)\n"
            "    if 'text' in answer:\n"
            "        sys.stdout.write(answer['text']); sys.exit(0)\n"
            "    sys.stdout.write(json.dumps(answer['json'])); sys.exit(0)\n"
            "sys.stderr.write('unexpected gh call: ' + argv + '\\n'); sys.exit(3)\n",
            encoding="utf-8",
        )
        fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
        return fake

    def case(self, prefix: str, *, graphql: Any = None, comments: Any = None,
             config: str | None = None, queue: bool = True,
             extra: list[tuple[str, dict[str, Any]]] | None = None) -> list[tuple[str, dict[str, Any]]]:
        config_answer = {"error": "gh: Not Found (HTTP 404)"} if config is None else {"text": config}
        return [
            ("contents/.shipyard/config.toml", config_answer),
            ("mergeQueue(branch", {"json": {"data": {"repository": {
                "mergeQueue": {"id": "MQ_1"} if queue else None}}}}),
            *(extra or []),
            ("graphql", {"json": graphql if graphql is not None
                         else review_fixture(f"{prefix}-graphql.json")}),
            ("/reviews", {"pages": review_fixture(f"{prefix}-reviews.json")}),
            ("/comments", {"pages": comments if comments is not None
                           else review_fixture(f"{prefix}-comments.json")}),
            ("/pulls/", {"json": review_fixture(f"{prefix}-pull.json")}),
        ]

    def run_guard(self, argv: list[str], answers: list[tuple[str, dict[str, Any]]],
                  repo: str, host: str | None = "1", **env: str) -> subprocess.CompletedProcess:
        fake = self.install(answers)
        environment = {"PATH": "/usr/bin:/bin", "GHAPP_REAL_GH": str(fake), "GH_REPO": repo,
                       "SHIPYARD_GHAPP_GUARDS_DIR": str(self.guards), "HOME": str(self.root)}
        if host is not None:
            environment["SHIPYARD_REVIEW_BEFORE_ARM"] = host
        environment.update(env)
        return subprocess.run(
            [sys.executable, str(self.guards / "queue-arm-guard"), *argv],
            env=environment, capture_output=True, text=True, check=False,
            stdin=subprocess.DEVNULL,
        )

    def arm(self, number: int, repo: str, answers: list[tuple[str, dict[str, Any]]],
            bind: str | None = None, **kwargs: Any) -> subprocess.CompletedProcess:
        head = bind or review_fixture(
            {513: "tartci-513", 511: "tartci-511", 490: "tartci-490", 784: "shipyard-784"}[number]
            + "-pull.json")["head"]["sha"]
        return self.run_guard(["pr", "merge", str(number), "--auto", "--match-head-commit", head],
                              answers, repo, **kwargs)

    def calls(self) -> str:
        return (self.root / "calls").read_text(encoding="utf-8")

    def approved_784(self, **kwargs: Any) -> list[tuple[str, dict[str, Any]]]:
        return self.case("shipyard-784", graphql=opened(review_fixture("shipyard-784-graphql.json")),
                         **kwargs)

    def test_no_verdict_is_refused(self) -> None:
        result = self.arm(513, "danielraffel/tartci", self.case("tartci-513"))
        self.assertEqual(result.returncode, 1, result.stderr)
        head = review_fixture("tartci-513-pull.json")["head"]["sha"]
        self.assertIn(f"no reviewer verdict names head {head[:12]}", result.stderr)
        self.assertIn(f"`verdict: APPROVE head:{head}` or `reviewed: {head}`", result.stderr)
        self.assertIn("from the fleet App or an OWNER, MEMBER or COLLABORATOR", result.stderr)

    def test_a_verdict_on_an_older_head_is_refused(self) -> None:
        result = self.arm(511, "danielraffel/tartci", self.case("tartci-511", extra=[
            ("compare/", {"error": "gh: Not Found (HTTP 404)"})]))
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("no reviewer verdict names head 52eda5490844", result.stderr)
        self.assertIn("The latest verdict names another head: `APPROVE head:0d7179d1", result.stderr)
        self.assertIn("The approval of 0d7179d1", result.stderr)

    def test_a_hold_naming_the_head_is_refused(self) -> None:
        result = self.arm(490, "danielraffel/tartci", self.case("tartci-490"))
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("the latest verdict naming head 82eeaab8a6bd is HOLD", result.stderr)

    def test_a_verdict_naming_the_head_is_allowed(self) -> None:
        # The App's APPROVE on the App's own PR counts: the author is the
        # shared fleet identity, so the two are not distinguishable.
        result = self.arm(784, "danielraffel/Shipyard", self.approved_784())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("repos/danielraffel/Shipyard/issues/784/comments", self.calls())
        self.assertIn("contents/.shipyard/config.toml?ref=main", self.calls())

    def test_a_hold_after_the_approve_is_refused(self) -> None:
        pages = review_fixture("shipyard-784-comments.json")
        approve = next(c for c in pages[0] if c["id"] == APPROVE_784_COMMENT)
        pages[0].append(comment_like(
            approve, id=approve["id"] + 1, created_at="2026-10-12T16:00:00Z",
            body=f"Delta review.\n\nverdict: HOLD head:{HEAD_784}"))
        result = self.arm(784, "danielraffel/Shipyard", self.approved_784(comments=pages))
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("the latest verdict naming head fd0e94e25f44 is HOLD", result.stderr)
        self.assertIn("2026-10-12T16:00:00Z", result.stderr)

    def test_a_hold_on_a_later_page_refuses(self) -> None:
        # Comments arrive 100 per page; the HOLD that decides may be on the last.
        pages = review_fixture("shipyard-784-comments.json")
        approve = next(c for c in pages[0] if c["id"] == APPROVE_784_COMMENT)
        hold = comment_like(approve, id=approve["id"] + 1, created_at="2026-10-12T16:00:00Z",
                            body=f"verdict: HOLD head:{HEAD_784}")
        filler = [comment_like(approve, id=approve["id"] + 10 + n, body="noted")
                  for n in range(100 - len(pages[0]))]
        result = self.arm(784, "danielraffel/Shipyard",
                          self.approved_784(comments=[pages[0] + filler, [hold]]))
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("is HOLD", result.stderr)
        self.assertIn("--paginate", self.calls())

    def test_a_drive_by_account_cannot_approve(self) -> None:
        pages = review_fixture("tartci-513-comments.json")
        head = review_fixture("tartci-513-pull.json")["head"]["sha"]
        template = pages[0][0]
        drive_by = comment_like(
            template, id=1, created_at="2026-10-12T23:00:00Z",
            user={"login": "some-random-github-user", "type": "User"},
            author_association="NONE", body=f"verdict: APPROVE head:{head}")
        result = self.arm(513, "danielraffel/tartci",
                          self.case("tartci-513", comments=[pages[0] + [drive_by]]))
        self.assertEqual(result.returncode, 1, result.stderr)
        # Control: the same comment from a collaborator approves.
        trusted = comment_like(drive_by, user={"login": "fable-graph", "type": "User"},
                               author_association="COLLABORATOR")
        result = self.arm(513, "danielraffel/tartci",
                          self.case("tartci-513", comments=[pages[0] + [trusted]]))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_the_request_must_bind_the_approved_head(self) -> None:
        unbound = self.run_guard(["pr", "merge", "784", "--auto"], self.approved_784(),
                                 "danielraffel/Shipyard")
        self.assertEqual(unbound.returncode, 1, unbound.stderr)
        self.assertIn("does not bind the head it lands", unbound.stderr)
        stale = self.arm(784, "danielraffel/Shipyard", self.approved_784(), bind=HOLD_784)
        self.assertEqual(stale.returncode, 1, stale.stderr)
        self.assertIn("binds cb5c1f796dac, but the head is fd0e94e25f44", stale.stderr)

    def fixes(self, stderr: str) -> list[dict[str, Any]]:
        prefix = "queue-arm-guard: fix: "
        return [json.loads(line[len(prefix):]) for line in stderr.splitlines()
                if line.startswith(prefix)]

    def test_every_review_refusal_says_what_to_do_next(self) -> None:
        head = review_fixture("tartci-513-pull.json")["head"]["sha"]
        unreviewed = self.arm(513, "danielraffel/tartci", self.case("tartci-513"))
        self.assertEqual(unreviewed.returncode, 1, unreviewed.stderr)
        [fix] = self.fixes(unreviewed.stderr)
        self.assertEqual(set(fix), {"what", "evidence", "cause", "command",
                                    "safe_to_auto_apply", "verify"})
        self.assertEqual(fix["cause"], "unreviewed")
        self.assertEqual(fix["evidence"], "https://github.com/danielraffel/tartci/pull/513")
        self.assertIn(f"`verdict: APPROVE head:{head}`", fix["command"])
        self.assertIs(fix["safe_to_auto_apply"], False, "approving is the reviewer's act")
        self.assertIn(head, fix["verify"])
        unbound = self.run_guard(["pr", "merge", "784", "--auto"], self.approved_784(),
                                 "danielraffel/Shipyard")
        [fix] = self.fixes(unbound.stderr)
        self.assertEqual(fix["cause"], "unbound")
        live = review_fixture("shipyard-784-pull.json")["head"]["sha"]
        self.assertIn(f"--match-head-commit {live}", fix["command"])
        self.assertIs(fix["safe_to_auto_apply"], False)
        self.assertIn("on the REST merge.", unbound.stderr)
        self.assertNotIn("Post the verdict", unbound.stderr)
        # An allowed landing prints no fix.
        pages = review_fixture("tartci-513-comments.json")
        verdict = comment_like(pages[0][0], id=2, created_at="2026-10-12T23:00:00Z",
                               body=f"verdict: APPROVE head:{head}")
        allowed = self.arm(513, "danielraffel/tartci",
                           self.case("tartci-513", comments=[pages[0] + [verdict]]))
        self.assertEqual(allowed.returncode, 0, allowed.stderr)
        self.assertEqual(self.fixes(allowed.stderr), [])

    def test_the_host_switch_decides_for_a_silent_repository(self) -> None:
        for host in ("0", None):
            result = self.arm(513, "danielraffel/tartci", self.case("tartci-513"), host=host)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotIn("/comments", self.calls())
            (self.root / "calls").unlink()
        (self.guards / "review-before-arm").write_text("", encoding="utf-8")
        result = self.arm(513, "danielraffel/tartci", self.case("tartci-513"), host=None)
        self.assertEqual(result.returncode, 1, result.stderr)

    def test_off_reads_nothing_even_with_a_config_this_python_cannot_parse(self) -> None:
        # Off must be inert on any Python, including one without tomllib, and
        # whatever the base config says: the review reads nothing at all.
        on = "[auto_merge]\narm_requires_head_approval = true\n[validation\n"
        for host in ("0", None):
            result = self.arm(513, "danielraffel/tartci", self.case("tartci-513", config=on),
                              host=host)
            self.assertEqual(result.returncode, 0, result.stderr)
            calls = self.calls()
            self.assertNotIn("contents/.shipyard/config.toml", calls)
            self.assertNotIn("/comments", calls)
            self.assertNotIn("/pulls/513", calls)
            (self.root / "calls").unlink()

    def test_a_repository_outside_the_list_with_no_setting_is_not_checked(self) -> None:
        graphql = copy.deepcopy(review_fixture("tartci-513-graphql.json"))
        graphql["data"]["repository"]["pullRequest"]["repository"]["nameWithOwner"] = "someone/else"
        result = self.arm(513, "someone/else", self.case("tartci-513", graphql=graphql))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("/comments", self.calls())

    @unittest.skipUnless(sys.version_info >= (3, 11), "reading the base config needs tomllib")
    def test_the_base_config_decides_over_the_default(self) -> None:
        off = "[auto_merge]\narm_requires_head_approval = false\n"
        result = self.arm(513, "danielraffel/tartci", self.case("tartci-513", config=off))
        self.assertEqual(result.returncode, 0, result.stderr)
        graphql = copy.deepcopy(review_fixture("tartci-513-graphql.json"))
        graphql["data"]["repository"]["pullRequest"]["repository"]["nameWithOwner"] = "someone/else"
        on = "[auto_merge]\narm_requires_head_approval = true\nreviewer_identities = [\"fable-graph\"]\n"
        result = self.arm(513, "someone/else", self.case("tartci-513", graphql=graphql, config=on),
                          host="1")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("from one of fable-graph", result.stderr)

    def test_a_config_that_does_not_parse_refuses(self) -> None:
        broken = "[auto_merge\narm_requires_head_approval = false\n"
        result = self.arm(513, "danielraffel/tartci", self.case("tartci-513", config=broken))
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("cannot read what its review requires", result.stderr)

    def test_the_operator_override_is_loud_and_recorded(self) -> None:
        result = self.arm(513, "danielraffel/tartci", self.case("tartci-513"),
                          GHAPP_ALLOW_UNREVIEWED_ARM="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("WARNING: GHAPP_ALLOW_UNREVIEWED_ARM=1 lands an unreviewed head", result.stderr)
        audit = [json.loads(line) for line in
                 (self.guards / "overrides.jsonl").read_text(encoding="utf-8").splitlines()]
        self.assertEqual(len(audit), 1)
        self.assertEqual(audit[0]["override"], "GHAPP_ALLOW_UNREVIEWED_ARM")
        self.assertIn("no reviewer verdict names head", audit[0]["refusal"])

    def test_the_queue_override_never_stands_in_for_a_review(self) -> None:
        # A queue refusal (armed, not yet queued) the operator overrides,
        # on a head nobody reviewed: still refused.
        graphql = copy.deepcopy(review_fixture("tartci-513-graphql.json"))
        graphql["data"]["repository"]["pullRequest"]["autoMergeRequest"] = {
            "enabledAt": "2026-10-12T22:00:00Z"}
        answers = self.case("tartci-513", graphql=graphql)
        refused = self.arm(513, "danielraffel/tartci", answers, host="0")
        self.assertEqual(refused.returncode, 1, refused.stderr)
        self.assertIn("is already armed since", refused.stderr)
        result = self.arm(513, "danielraffel/tartci", answers, GHAPP_ALLOW_QUEUE_REARM="1")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("refusing to arm or merge an unreviewed head", result.stderr)
        # Control: with a verdict on the head the queue override lands it.
        head = review_fixture("tartci-513-pull.json")["head"]["sha"]
        pages = review_fixture("tartci-513-comments.json")
        verdict = comment_like(pages[0][0], id=2, created_at="2026-10-12T23:00:00Z",
                               body=f"verdict: APPROVE head:{head}")
        answers = self.case("tartci-513", graphql=graphql, comments=[pages[0] + [verdict]])
        result = self.arm(513, "danielraffel/tartci", answers, GHAPP_ALLOW_QUEUE_REARM="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("GHAPP_ALLOW_QUEUE_REARM=1 overrides a refusal", result.stderr)

    def test_an_ambiguous_request_needs_both_overrides(self) -> None:
        argv = ["api", "graphql", "--input", "-"]
        result = self.run_guard(argv, [], "danielraffel/tartci", GHAPP_ALLOW_QUEUE_REARM="1")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("GHAPP_ALLOW_UNREVIEWED_ARM=1 is needed as well", result.stderr)
        result = self.run_guard(argv, [], "danielraffel/tartci", GHAPP_ALLOW_QUEUE_REARM="1",
                                GHAPP_ALLOW_UNREVIEWED_ARM="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.guards / "overrides.jsonl").is_file())

    def test_an_unreadable_review_trail_refuses(self) -> None:
        answers = self.case("tartci-513", extra=[("/comments", {"error": "HTTP 502"})])
        result = self.arm(513, "danielraffel/tartci", answers)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("cannot read what its review requires", result.stderr)

    # Direct merges: on a queue-less base a merge is the landing path.

    def merge_paths(self, head: str) -> dict[str, list[str]]:
        body = self.root / "merge.json"
        body.write_text(json.dumps({"sha": head, "merge_method": "merge"}), encoding="utf-8")
        mutation = ("query=mutation{mergePullRequest(input:{pullRequestId:\"PR_784\","
                    f"expectedHeadOid:\"{head}\"}}){{pullRequest{{number}}}}}}")
        return {
            "pr merge": ["pr", "merge", "784", "--merge", "--match-head-commit", head],
            "pr merge --admin": ["pr", "merge", "784", "--admin", "--match-head-commit", head],
            "REST -f sha": ["api", "-X", "PUT", "repos/danielraffel/Shipyard/pulls/784/merge",
                            "-f", f"sha={head}"],
            "REST --input": ["api", "-X", "PUT", "repos/danielraffel/Shipyard/pulls/784/merge",
                             "--input", str(body)],
            "mergePullRequest": ["api", "graphql", "-f", mutation],
        }

    def test_direct_merges_need_the_verdict_too(self) -> None:
        pages = review_fixture("shipyard-784-comments.json")
        approve = next(c for c in pages[0] if c["id"] == APPROVE_784_COMMENT)
        held = [pages[0] + [comment_like(approve, id=approve["id"] + 1,
                                         created_at="2026-10-12T16:00:00Z",
                                         body=f"verdict: HOLD head:{HEAD_784}")]]
        for path, argv in self.merge_paths(HEAD_784).items():
            with self.subTest(path=path):
                allowed = self.run_guard(argv, self.approved_784(queue=False), "danielraffel/Shipyard")
                self.assertEqual(allowed.returncode, 0, allowed.stderr)
                refused = self.run_guard(argv, self.approved_784(queue=False, comments=held),
                                         "danielraffel/Shipyard")
                self.assertEqual(refused.returncode, 1, refused.stderr)
                self.assertIn("is HOLD", refused.stderr)

    def test_direct_merges_must_bind_the_approved_head(self) -> None:
        for path, argv in self.merge_paths(HOLD_784).items():
            with self.subTest(path=path):
                stale = self.run_guard(argv, self.approved_784(queue=False), "danielraffel/Shipyard")
                self.assertEqual(stale.returncode, 1, stale.stderr)
                self.assertIn("binds cb5c1f796dac, but the head is fd0e94e25f44", stale.stderr)
        unbound = {
            "pr merge": ["pr", "merge", "784", "--merge"],
            "REST": ["api", "-X", "PUT", "repos/danielraffel/Shipyard/pulls/784/merge"],
            "mergePullRequest": ["api", "graphql", "-f",
                                 "query=mutation{mergePullRequest(input:{pullRequestId:\"PR_784\"})"
                                 "{pullRequest{number}}}"],
        }
        for path, argv in unbound.items():
            with self.subTest(path=path):
                result = self.run_guard(argv, self.approved_784(queue=False), "danielraffel/Shipyard")
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("does not bind the head it lands", result.stderr)

    def test_a_read_of_the_merge_endpoint_is_not_a_merge(self) -> None:
        result = self.run_guard(["api", "repos/danielraffel/Shipyard/pulls/784/merge"], [],
                                "danielraffel/Shipyard")
        self.assertEqual(result.returncode, 0, result.stderr)

    # A merge-only update of an approved head, over real crafted git histories
    # (the cases of the reviewer's carry_probe.sh).

    def carry_answers(self, history: "CarryHistory", head: str, comments: list | None = None
                      ) -> list[tuple[str, dict[str, Any]]]:
        graphql = opened(review_fixture("shipyard-784-graphql.json"))
        graphql["data"]["repository"]["pullRequest"]["headRefOid"] = head
        pull = copy.deepcopy(review_fixture("shipyard-784-pull.json"))
        pull["head"]["sha"] = head
        approve = next(c for c in review_fixture("shipyard-784-comments.json")[0]
                       if c["id"] == APPROVE_784_COMMENT)
        trail = comments if comments is not None else [comment_like(
            approve, created_at="2026-10-12T15:00:00Z",
            body=f"verdict: APPROVE head:{history.approved}")]
        answers = self.case("shipyard-784", graphql=graphql, queue=False, comments=[trail])
        answers.insert(0, ("compare/", {"git": str(history.path)}))
        answers.insert(0, ("object(expression:", {"git_tree": str(history.path)}))
        return [(needle, {"json": pull}) if needle == "/pulls/" else (needle, answer)
                for needle, answer in answers]

    def land(self, history: "CarryHistory", head: str, comments: list | None = None
             ) -> subprocess.CompletedProcess:
        return self.run_guard(["pr", "merge", "784", "--merge", "--match-head-commit", head],
                              self.carry_answers(history, head, comments), "danielraffel/Shipyard")

    def test_an_honest_merge_of_the_base_carries_the_approval(self) -> None:
        history = CarryHistory(self.root / "repo")
        result = self.land(history, history.merge())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"compare/main...{history.approved}", self.calls())

    def test_a_merge_that_moves_the_prs_guard_line_needs_a_new_verdict(self) -> None:
        history = CarryHistory(self.root / "repo")
        head = history.merge(edit=lambda: history.move_guard_line(49))
        result = self.land(history, head)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("guard.c at", result.stderr)
        self.assertIn("is not the blob approved", result.stderr)

    def test_a_merge_that_swaps_a_binary_the_pr_touches_needs_a_new_verdict(self) -> None:
        history = CarryHistory(self.root / "repo", binary=True)
        head = history.merge(edit=lambda: history.write("x.bin", b"\x00\x01EVIL-binary"))
        result = self.land(history, head)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("changes a binary file (x.bin)", result.stderr)

    def test_a_merge_that_only_flips_a_prs_file_mode_needs_a_new_verdict(self) -> None:
        # The compare API lists no mode, so the blob alone looks unchanged.
        history = CarryHistory(self.root / "repo")
        head = history.merge(edit=lambda: (history.path / "guard.c").chmod(0o755))
        self.assertEqual(history.git("rev-parse", f"{head}:guard.c"),
                         history.git("rev-parse", f"{history.approved}:guard.c"))
        self.assertEqual(history.git("ls-tree", head, "guard.c").split()[0], "100755")
        self.assertEqual(history.git("ls-tree", history.approved, "guard.c").split()[0], "100644")
        result = self.land(history, head)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("guard.c at", result.stderr)
        self.assertIn("does not keep the mode approved", result.stderr)
        # Control: the honest merge reads the same trees and carries.
        self.assertEqual(self.land(history, history.merge()).returncode, 0)

    def test_a_merge_that_adds_a_line_needs_a_new_verdict(self) -> None:
        history = CarryHistory(self.root / "repo")
        head = history.merge(edit=lambda: history.append("guard.c", "  backdoor();\n"))
        result = self.land(history, head)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("is not the blob approved", result.stderr)

    def test_a_file_both_the_pr_and_the_base_changed_needs_a_new_verdict(self) -> None:
        # The base also edits guard.c; the merge keeps the PR's blob, which
        # quietly drops the base's change.
        history = CarryHistory(self.root / "repo")
        history.on_base(lambda: history.append("guard.c", "base head\n", at_start=True))
        head = history.merge(edit=lambda: history.restore("guard.c", history.approved))
        result = self.land(history, head)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("guard.c was changed by both the pull request and the base", result.stderr)

    def test_a_hold_on_the_new_head_is_not_overridden_by_a_carry(self) -> None:
        history = CarryHistory(self.root / "repo")
        head = history.merge()
        approve = next(c for c in review_fixture("shipyard-784-comments.json")[0]
                       if c["id"] == APPROVE_784_COMMENT)
        trail = [
            comment_like(approve, created_at="2026-10-12T15:00:00Z",
                         body=f"verdict: APPROVE head:{history.approved}"),
            comment_like(approve, id=approve["id"] + 1, created_at="2026-10-12T16:00:00Z",
                         body=f"verdict: HOLD head:{head}"),
        ]
        result = self.land(history, head, trail)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("is HOLD", result.stderr)
        # Control: the same honest merge without the HOLD carries.
        self.assertEqual(self.land(history, head).returncode, 0)

    def test_a_head_that_does_not_descend_from_the_approved_one_needs_a_new_verdict(self) -> None:
        history = CarryHistory(self.root / "repo")
        head = history.rewrite()
        result = self.land(history, head)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(f"does not descend from {history.approved[:12]}", result.stderr)

    # A rebase of an approved head carries only behind its own host switch,
    # and only when all four conditions hold.

    def rebase_answers(self, history: "CarryHistory", head: str,
                       conclusion: str = "success") -> list[tuple[str, dict[str, Any]]]:
        answers = self.carry_answers(history, head)
        answers.insert(0, ("git/blobs/", {"git_blob": str(history.path)}))
        answers.insert(0, ("rules/branches/", {"json": [
            {"type": "required_status_checks",
             "parameters": {"required_status_checks": [{"context": "macos"}]}}]}))
        answers.insert(0, ("/protection/required_status_checks",
                           {"error": "gh: Branch not protected (HTTP 404)"}))
        answers.insert(0, ("/check-runs", {"json": {"total_count": 1, "check_runs": [
            {"id": 7, "name": "macos", "status": "completed", "conclusion": conclusion}]}}))
        return answers

    def land_rebased(self, history: "CarryHistory", head: str, switch: str | None = "1",
                     conclusion: str = "success") -> subprocess.CompletedProcess:
        env = {} if switch is None else {"SHIPYARD_REBASE_CARRY": switch}
        return self.run_guard(["pr", "merge", "784", "--merge", "--match-head-commit", head],
                              self.rebase_answers(history, head, conclusion),
                              "danielraffel/Shipyard", **env)

    def carries(self) -> list[dict[str, Any]]:
        log = self.guards / "carries.jsonl"
        if not log.is_file():
            return []
        return [json.loads(line) for line in log.read_text(encoding="utf-8").splitlines()]

    def test_a_clean_rebase_carries_only_with_the_switch_on_and_is_logged(self) -> None:
        history = CarryHistory(self.root / "repo")
        head = history.rebase()
        off = self.land_rebased(history, head, switch=None)
        self.assertEqual(off.returncode, 1, off.stderr)
        self.assertIn(f"does not descend from {history.approved[:12]}", off.stderr)
        self.assertEqual(self.carries(), [])
        forced_off = self.land_rebased(history, head, switch="0")
        self.assertEqual(forced_off.returncode, 1, forced_off.stderr)
        on = self.land_rebased(history, head)
        self.assertEqual(on.returncode, 0, on.stderr)
        self.assertIn("carried across a rebase", on.stderr)
        [record] = self.carries()
        self.assertEqual((record["approved"], record["head"], record["pr"], record["kind"]),
                         (history.approved, head, 784, "rebase"))
        self.assertTrue(record["patch_id"])
        self.assertNotEqual(record["old_base"], record["new_base"])
        # The file switch turns it on too.
        (self.guards / "rebase-carry").write_text("", encoding="utf-8")
        self.assertEqual(self.land_rebased(history, head, switch=None).returncode, 0)

    def test_a_rebase_over_a_base_edit_to_the_same_file_reapplies_cleanly(self) -> None:
        history = CarryHistory(self.root / "repo")
        history.on_base(lambda: history.replace_line(50, "line fifty (base)\n"))
        head = history.rebase()
        result = self.land_rebased(history, head)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("git/blobs/", self.calls(), "the overlap was re-applied, not assumed")

    def test_a_hand_resolved_conflict_does_not_carry(self) -> None:
        history = CarryHistory(self.root / "repo")
        history.on_base(lambda: history.replace_line(10, "line ten (base)\n"))
        head = history.rebase(resolve=lambda: history.write(
            "guard.c", (history.path / "guard.c").read_text().split("<<<<<<<")[0].encode()
            + b"  if (!authorized) return;\nline ten (base)\n"
            + "".join(f"line {n}\n" for n in range(11, 61)).encode()))
        result = self.land_rebased(history, head)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("does not apply cleanly to the new base", result.stderr)

    def test_a_rebase_that_changes_the_patch_does_not_carry(self) -> None:
        history = CarryHistory(self.root / "repo")
        head = history.rebase(after=lambda: history.append("guard.c", "  backdoor();\n"))
        result = self.land_rebased(history, head)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("the diff's patch-id changed", result.stderr)
        self.assertIn("re-applying the approved change to guard.c", result.stderr)

    def test_a_rebase_that_adds_a_file_does_not_carry(self) -> None:
        history = CarryHistory(self.root / "repo")
        head = history.rebase(after=lambda: history.write("notes.txt", b"new\n"))
        result = self.land_rebased(history, head)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("the set of files the pull request changes moved (notes.txt)", result.stderr)

    def test_a_rebase_that_flips_a_mode_does_not_carry(self) -> None:
        history = CarryHistory(self.root / "repo")
        head = history.rebase(after=lambda: (history.path / "guard.c").chmod(0o755))
        result = self.land_rebased(history, head)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("does not keep the mode approved", result.stderr)

    def test_a_skipped_required_check_does_not_carry(self) -> None:
        history = CarryHistory(self.root / "repo")
        head = history.rebase()
        for conclusion in ("skipped", "neutral"):
            with self.subTest(conclusion=conclusion):
                result = self.land_rebased(history, head, conclusion=conclusion)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn(f"required check `macos` on {head[:12]} concluded `{conclusion}`",
                              result.stderr)
        self.assertEqual(self.carries(), [])

    def test_a_commit_that_is_not_a_merge_needs_a_new_verdict(self) -> None:
        history = CarryHistory(self.root / "repo")
        history.git("checkout", "-q", "-B", "single", history.approved)
        history.append("notes.txt", "note\n")
        history.git("add", "-A")
        history.git("commit", "-qm", "single parent")
        result = self.land(history, history.git("rev-parse", "HEAD"))
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("is not a merge", result.stderr)


class CarryHistory:
    """A scratch git repository: a base, a pull request that adds a guard line
    (and optionally changes a binary), its approved head, and a base commit
    since; with the updates the carry rule judges."""

    def __init__(self, path: pathlib.Path, binary: bool = False) -> None:
        self.path = path
        path.mkdir(parents=True)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "r@r")
        self.git("config", "user.name", "r")
        self.write("guard.c", "".join(f"line {n}\n" for n in range(1, 61)).encode())
        if binary:
            self.write("x.bin", b"\x00\x01base-binary")
        self.git("add", "-A")
        self.git("commit", "-qm", "base0")
        self.git("checkout", "-qb", "pr")
        lines = (self.path / "guard.c").read_text().splitlines(keepends=True)
        lines.insert(9, "  if (!authorized) return;\n")
        self.write("guard.c", "".join(lines).encode())
        if binary:
            self.write("x.bin", b"\x00\x01pr-binary-v1")
        self.git("commit", "-qam", "pr: add guard")
        self.approved = self.git("rev-parse", "HEAD")
        self.git("checkout", "-q", "main")
        self.write("other.txt", b"unrelated\n")
        self.git("add", "other.txt")
        self.git("commit", "-qm", "base1")

    def git(self, *args: str) -> str:
        return subprocess.run(["git", *args], cwd=self.path, capture_output=True, text=True,
                              check=True).stdout.strip()

    def write(self, name: str, data: bytes) -> None:
        (self.path / name).write_bytes(data)

    def append(self, name: str, text: str, at_start: bool = False) -> None:
        old = (self.path / name).read_text() if (self.path / name).exists() else ""
        (self.path / name).write_text(text + old if at_start else old + text)

    def restore(self, name: str, commit: str) -> None:
        self.git("checkout", commit, "--", name)

    def move_guard_line(self, to: int) -> None:
        lines = [line for line in (self.path / "guard.c").read_text().splitlines(keepends=True)
                 if "authorized" not in line]
        lines.insert(to - 1, "  if (!authorized) return;\n")
        self.write("guard.c", "".join(lines).encode())

    def on_base(self, edit: Any) -> None:
        self.git("checkout", "-q", "main")
        edit()
        self.git("commit", "-qam", "base edit")

    def merge(self, edit: Any = None) -> str:
        """Merge the base into the approved head, optionally editing the merge."""
        self.git("checkout", "-q", "-B", "head", self.approved)
        self.git("merge", "-q", "--no-commit", "main")
        if edit is not None:
            edit()
        self.git("add", "-A")
        self.git("commit", "-qm", "Merge branch 'main' into pr")
        return self.git("rev-parse", "HEAD")

    def replace_line(self, number: int, text: str) -> None:
        lines = (self.path / "guard.c").read_text().splitlines(keepends=True)
        index = next(i for i, line in enumerate(lines) if line == f"line {number}\n")
        lines[index] = text
        self.write("guard.c", "".join(lines).encode())

    def rebase(self, after: Any = None, resolve: Any = None) -> str:
        """Rebase the approved head onto the base; ``resolve`` settles a
        conflict by hand, ``after`` amends the rebased head."""
        self.git("checkout", "-q", "-B", "rebased", self.approved)
        done = subprocess.run(["git", "rebase", "-q", "main"], cwd=self.path,
                              capture_output=True, text=True)
        if done.returncode != 0:
            if resolve is None:
                raise AssertionError(f"unexpected conflict: {done.stdout}{done.stderr}")
            resolve()
            self.git("add", "-A")
            subprocess.run(["git", "-c", "core.editor=true", "rebase", "--continue"],
                           cwd=self.path, capture_output=True, text=True, check=True)
        elif resolve is not None:
            raise AssertionError("the rebase was expected to conflict")
        if after is not None:
            after()
            self.git("add", "-A")
            self.git("commit", "-q", "--amend", "--no-edit")
        return self.git("rev-parse", "HEAD")

    def rewrite(self) -> str:
        """The pull request's change re-made on the base: same content, a
        head that does not descend from the approved one."""
        self.git("checkout", "-q", "-B", "rewritten", "main")
        self.restore("guard.c", self.approved)
        self.git("commit", "-qam", "pr: add guard (rebased)")
        return self.git("rev-parse", "HEAD")


class TreeModes(unittest.TestCase):
    """Each file's mode is its own entry's, even behind a sibling with another mode."""

    def test_each_file_reads_its_own_entry(self) -> None:
        queries: list[str] = []

        def fake(arguments: list[str]) -> Any:
            queries.append(" ".join(arguments))
            return json.loads('''{"data": {"repository": {
            "d0": {"entries": [{"name": "a.sh", "mode": 33261}, {"name": "guard.c", "mode": 33188}]},
            "d1": {"entries": [{"name": "run", "mode": 33261}, {"name": "tool.py", "mode": 33188}]}}}}''')

        with mock.patch.object(guard, "run_real_gh", side_effect=fake):
            modes = guard.tree_modes("o/r", "abc", ["guard.c", "bin/sub/tool.py", "bin/sub/gone.txt"])
        self.assertIn('d0:object(expression:"abc:")', queries[0])
        self.assertIn('d1:object(expression:"abc:bin/sub")', queries[0])
        self.assertEqual(modes, {"guard.c": 0o100644, "bin/sub/tool.py": 0o100644,
                                 "bin/sub/gone.txt": None})


class ReviewIdentity(unittest.TestCase):
    """Who may approve, in-process."""

    HEAD = "a" * 40

    def comment(self, login: str, body: str, at: str | None = "2026-10-12T01:00:00Z",
                association: str = "COLLABORATOR") -> dict[str, Any]:
        return {"user": {"login": login}, "created_at": at, "body": body,
                "author_association": association}

    def policy(self) -> dict[str, Any]:
        return guard.review_policy(None, "danielraffel/tartci", True)

    def test_a_distinguishable_author_cannot_approve_their_own_head(self) -> None:
        mine = [self.comment("danielraffel", f"verdict: APPROVE head:{self.HEAD}", association="OWNER")]
        allowed, why = guard.review_verdict(self.HEAD, "danielraffel", mine, [], self.policy())
        self.assertFalse(allowed, why)
        self.assertIn("other than the PR author danielraffel", why)
        theirs = [self.comment("fable-graph", f"reviewed: {self.HEAD}")]
        self.assertTrue(guard.review_verdict(self.HEAD, "danielraffel", theirs, [], self.policy())[0])

    def test_only_the_fleet_app_and_trusted_accounts_count(self) -> None:
        for association in ("NONE", "CONTRIBUTOR", "FIRST_TIME_CONTRIBUTOR", None):
            outsider = [self.comment("drive-by", f"reviewed: {self.HEAD}", association=association)]
            self.assertFalse(
                guard.review_verdict(self.HEAD, "shipyard-local[bot]", outsider, [], self.policy())[0],
                association)
        fleet = [self.comment("shipyard-local[bot]", f"reviewed: {self.HEAD}", association="CONTRIBUTOR")]
        self.assertTrue(guard.review_verdict(self.HEAD, "shipyard-local[bot]", fleet, [], self.policy())[0])

    def test_an_approved_review_on_the_head_counts(self) -> None:
        review = {"user": {"login": "fable-graph"}, "state": "APPROVED", "commit_id": self.HEAD,
                  "submitted_at": "2026-10-12T01:00:00Z", "author_association": "MEMBER"}
        self.assertTrue(guard.review_verdict(self.HEAD, "shipyard-local[bot]", [], [review], self.policy())[0])
        hold = dict(review, state="CHANGES_REQUESTED", submitted_at="2026-10-12T02:00:00Z")
        self.assertFalse(guard.review_verdict(self.HEAD, "shipyard-local[bot]", [], [review, hold], self.policy())[0])

    def test_a_hold_with_no_readable_time_refuses_beside_a_newer_approval(self) -> None:
        comments = [
            self.comment("fable-graph", f"verdict: HOLD head:{self.HEAD}", at=None),
            self.comment("fable-graph", f"verdict: APPROVE head:{self.HEAD}", at="2026-10-12T05:00:00Z"),
        ]
        allowed, why = guard.review_verdict(self.HEAD, "shipyard-local[bot]", comments, [], self.policy())
        self.assertFalse(allowed, why)
        self.assertIn("has no readable time", why)

    def test_only_the_last_line_is_a_verdict(self) -> None:
        self.assertIsNone(guard.marker_verdict(f"verdict: APPROVE head:{self.HEAD}\nthen more"))
        self.assertEqual(guard.marker_verdict(f"prose\n\nreviewed: {self.HEAD}\n"), ("APPROVE", self.HEAD))

    def test_without_tomllib_an_existing_config_refuses(self) -> None:
        off = "[auto_merge]\narm_requires_head_approval = false\n"
        with mock.patch.dict(sys.modules, {"tomllib": None}):
            with self.assertRaises(guard.GuardError):
                guard.review_policy(off, "Generous-Corp/pulp", True)
        self.assertFalse(guard.review_policy(None, "Generous-Corp/pulp", False)["required"])
        self.assertTrue(guard.review_policy(None, "Generous-Corp/pulp", True)["required"])

    def test_the_default_repositories_match_the_rust_list(self) -> None:
        source = (SCRIPT.parent.parent / "src" / "head_approval.rs").read_text(encoding="utf-8")
        block = source.split("pub const REVIEW_REQUIRED_REPOS", 1)[1].split("];", 1)[0]
        rust = tuple(re.findall(r'"([^"]+)"', block))
        self.assertEqual(rust, guard.REVIEW_REQUIRED_REPOS)
        self.assertEqual(len(rust), 4)
        trusted = source.split("pub const TRUSTED_ASSOCIATIONS", 1)[1].split("];", 1)[0]
        self.assertEqual(tuple(re.findall(r'"([^"]+)"', trusted)), guard.TRUSTED_ASSOCIATIONS)

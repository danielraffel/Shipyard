from __future__ import annotations

import contextlib
import copy
import importlib.util
import io
import json
import os
import pathlib
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
                for key in ("entry_state", "position", "enabled_at"):
                    if key in want:
                        self.assertEqual(got[key], want[key])
                allowed, _ = guard.decide(got)
                self.assertEqual("allow" if allowed else "refuse", want["guard"])
                checked += 1
        # Control: the corpus was actually visited.
        self.assertEqual(checked, 6)

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
        pr["timelineItems"]["nodes"].append({"__typename": "HeadRefForcePushedEvent"})
        self.assertTrue(guard.classify_pr_queue_state(response)["new_head_since_removal"])

    def test_unreadable_is_unknown(self) -> None:
        for value in ({}, {"errors": [{"message": "x"}]}, {"data": {"node": {"state": "OPEN"}}}):
            with self.subTest(value=value):
                self.assertEqual(guard.classify_pr_queue_state(value)["class"], "unknown")


class QueueArmGuardTests(unittest.TestCase):
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
            ["pr", "merge", "7", "--admin", "--merge"],
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

    def test_same_head_ejection_is_refused_and_names_the_override(self) -> None:
        response = fixture("pr_ejected_requeued.json")
        pr = response["data"]["repository"]["pullRequest"]
        pr["isInMergeQueue"] = False
        pr["mergeQueueEntry"] = None
        pr["timelineItems"]["nodes"].pop()
        code, message, _ = self.run_guard(["pr", "merge", "8702", "--auto"], [response])
        self.assertEqual(code, 1)
        self.assertIn("ejected for failed_checks at 2026-09-22T20:29:00Z", message)
        self.assertIn("push a fix first", message)
        self.assertIn("GHAPP_ALLOW_QUEUE_REARM=1", message)

    def test_ejection_with_new_head_is_allowed(self) -> None:
        response = fixture("pr_ejected_requeued.json")
        pr = response["data"]["repository"]["pullRequest"]
        pr["isInMergeQueue"] = False
        pr["mergeQueueEntry"] = None
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


if __name__ == "__main__":
    unittest.main()

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
                for key in ("entry_state", "position", "enabled_at", "reason", "new_head_since_removal"):
                    if key in want:
                        self.assertEqual(got[key], want[key])
                allowed, _ = guard.decide(got)
                self.assertEqual("allow" if allowed else "refuse", want["guard"])
                checked += 1
        # Control: the whole corpus, real and labelled-synthetic, was visited.
        self.assertEqual(checked, 10)

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

    def declare(self, verdict: Any, *, exit_code: int = 0, command: Any = None) -> None:
        """Write a `.shipyard/config.toml` declaring a stub attributor."""
        (self.root / ".shipyard").mkdir(parents=True, exist_ok=True)
        script = self.root / "attribute.py"
        body = verdict if isinstance(verdict, str) else json.dumps(verdict)
        script.write_text(
            "import sys, pathlib\n"
            f"pathlib.Path({str(self.root / 'argv')!r}).write_text(' '.join(sys.argv[1:]))\n"
            f"sys.stdout.write({body!r})\n"
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
        self.assertEqual(message, "")
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

    def test_jobs_are_read_without_gh_paginate(self) -> None:
        """`gh api --paginate` concatenates one object per page, which is not JSON."""
        self.declare(self.certifies())
        _, _, calls = self.run_guard([fixture(self.INCIDENT), *self.batch_reads()])
        jobs_call = calls[2]
        self.assertNotIn("--paginate", jobs_call)
        self.assertIn("per_page=100", jobs_call[1])

    def test_certification_never_names_the_override(self) -> None:
        for verdict in (self.certifies(), {"run_id": self.RUN_ID, "implicates_head": None}):
            with self.subTest(verdict=verdict):
                self.declare(verdict)
                _, message, _ = self.run_guard([fixture(self.INCIDENT), *self.batch_reads()])
                for name in ("GHAPP_ALLOW_QUEUE_REARM", "SHIPYARD_INTERNAL_QUEUE_MUTATION"):
                    self.assertNotIn(name, message)


if __name__ == "__main__":
    unittest.main()

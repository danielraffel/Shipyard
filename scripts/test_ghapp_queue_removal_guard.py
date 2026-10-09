from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


SCRIPT = pathlib.Path(__file__).with_name("ghapp_queue_removal_guard.py")
SPEC = importlib.util.spec_from_file_location("ghapp_queue_removal_guard", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)


class QueueRemovalGuardTests(unittest.TestCase):
    def run_guard(self, args: list[str], **env: str) -> tuple[int, str]:
        stderr = io.StringIO()
        with mock.patch.dict(os.environ, env, clear=True), contextlib.redirect_stderr(stderr):
            return guard.main(args), stderr.getvalue()

    def test_harmless_commands_are_allowed(self) -> None:
        self.assertEqual(self.run_guard(["pr", "view", "7476"])[0], 0)
        self.assertEqual(
            self.run_guard(["api", "graphql", "-f", "query=query { viewer { login } }"])[0],
            0,
        )
        self.assertEqual(self.run_guard(["pr", "merge", "7476", "--auto"])[0], 0)

    def test_disable_auto_is_refused_without_explicit_authority(self) -> None:
        code, message = self.run_guard(["pr", "merge", "7476", "--disable-auto"])
        self.assertEqual(code, 1)
        self.assertIn("refusing unaudited", message)
        for name in ("GHAPP_ALLOW_QUEUE_REMOVAL", "SHIPYARD_INTERNAL_QUEUE_MUTATION"):
            self.assertNotIn(name, message)

    def test_raw_dequeue_and_disable_mutations_are_refused(self) -> None:
        for mutation in ("dequeuePullRequest", "disablePullRequestAutoMerge"):
            with self.subTest(mutation=mutation):
                code, _ = self.run_guard(
                    [
                        "api",
                        "graphql",
                        "-f",
                        f"query=mutation($id:ID!) {{{mutation}(input:{{id:$id}}) {{ clientMutationId }} }}",
                    ]
                )
                self.assertEqual(code, 1)

    def test_input_file_mutation_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            request = pathlib.Path(directory) / "request.json"
            request.write_text(
                r'{"query":"mutation { dequeuePullRequest(input:{id:\"x\"}) { clientMutationId } }"}'
            )
            self.assertEqual(self.run_guard(["api", "graphql", "--input", str(request)])[0], 1)

    def test_input_file_decodes_escaped_mutation_name(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            request = pathlib.Path(directory) / "request.json"
            request.write_text(
                r'{"query":"mutation { dequeuePull\u0052equest(input:{id:\"x\"}) { clientMutationId } }"}'
            )
            self.assertEqual(self.run_guard(["api", "graphql", "--input", str(request)])[0], 1)

    def test_graphql_endpoint_is_found_after_flags(self) -> None:
        args = [
            "api",
            "--method",
            "POST",
            "graphql",
            "-fquery=mutation { dequeuePullRequest(input:{id:\"x\"}) { clientMutationId } }",
        ]
        self.assertEqual(self.run_guard(args)[0], 1)

    def test_graphql_stdin_bodies_fail_closed(self) -> None:
        for args in (
            ["api", "graphql", "--input", "-"],
            ["api", "graphql", "--input=-"],
            ["api", "graphql", "-Fquery=@-"],
        ):
            with self.subTest(args=args):
                code, message = self.run_guard(args)
                self.assertEqual(code, 1)
                self.assertIn("stdin", message)

    def test_endpoint_query_mutation_is_refused(self) -> None:
        endpoint = (
            "graphql?query=mutation%20%7B%20disablePullRequestAutoMerge"
            "%28input:%7BpullRequestId:%22x%22%7D%29%20%7BclientMutationId%7D%20%7D"
        )
        self.assertEqual(self.run_guard(["api", endpoint])[0], 1)

    def test_ambiguous_graphql_endpoint_spellings_fail_closed(self) -> None:
        mutation = "query=mutation { dequeuePullRequest(input:{id:\"x\"}) { clientMutationId } }"
        for endpoint in ("%67raphql", "./graphql"):
            with self.subTest(endpoint=endpoint):
                code, message = self.run_guard(["api", endpoint, "-f", mutation])
                self.assertEqual(code, 1)
                self.assertIn("refusing unaudited", message)

    def test_encoded_unrelated_rest_endpoint_is_allowed(self) -> None:
        self.assertEqual(self.run_guard(["api", "repos/o/r/contents/a%20b"])[0], 0)

    def test_trusted_absolute_github_endpoint_is_allowed(self) -> None:
        self.assertEqual(
            self.run_guard(["api", "https://api.github.com/repos/o/r"])[0], 0
        )

    def test_unrelated_rest_input_body_is_not_parsed_as_graphql(self) -> None:
        self.assertEqual(self.run_guard(["api", "repos/o/r/issues", "--input", "-"])[0], 0)

    def test_endpoint_placeholder_with_removal_mutation_fails_closed(self) -> None:
        args = [
            "api",
            "{owner}",
            "-f",
            "query=mutation { dequeuePullRequest(input:{id:\"x\"}) { clientMutationId } }",
        ]
        code, message = self.run_guard(args, GH_REPO="graphql/x")
        self.assertEqual(code, 1)
        self.assertIn("refusing unaudited", message)

    def test_repository_placeholders_fall_back_to_git_remote(self) -> None:
        completed = subprocess.CompletedProcess(
            ["/opt/homebrew/bin/gh", "repo", "view", "--json", "nameWithOwner"],
            0,
            stdout='{"nameWithOwner":"o/r"}\n',
            stderr="",
        )
        with mock.patch.object(guard.subprocess, "run", return_value=completed) as run:
            self.assertEqual(self.run_guard(["api", "repos/{owner}/{repo}"])[0], 0)
        self.assertEqual(run.call_args.kwargs["timeout"], 30)

    def test_repository_placeholder_resolution_timeout_fails_closed(self) -> None:
        with mock.patch.object(
            guard.subprocess,
            "run",
            side_effect=subprocess.TimeoutExpired(["gh", "repo", "view"], 30),
        ):
            code, message = self.run_guard(["api", "repos/{owner}/{repo}"])
        self.assertEqual(code, 1)
        self.assertIn("cannot resolve current repository identity", message)

    def test_override_refuses_ambiguous_input_it_cannot_read(self) -> None:
        code, message = self.run_guard(
            ["api", "graphql", "--input", "-"], GHAPP_ALLOW_QUEUE_REMOVAL="1"
        )
        self.assertEqual(code, 1)
        self.assertIn("ambiguous", message)

    def test_compact_field_and_query_file_mutations_are_refused(self) -> None:
        mutation = "mutation { dequeuePullRequest(input:{id:\"x\"}) { clientMutationId } }"
        self.assertEqual(
            self.run_guard(["api", "graphql", f"-fquery={mutation}"])[0],
            1,
        )
        with tempfile.TemporaryDirectory() as directory:
            request = pathlib.Path(directory) / "mutation.graphql"
            request.write_text(mutation)
            self.assertEqual(
                self.run_guard(["api", "graphql", "-f", f"query=@{request}"])[0],
                1,
            )

    def test_internal_marker_is_honoured_only_from_a_shipyard_parent(self) -> None:
        args = ["api", "graphql", "-f", "query=mutation { dequeuePullRequest(input:{id:\"x\"}) { clientMutationId } }"]
        with mock.patch.object(guard, "process_ancestry", return_value=["bash", "shipyard"]):
            self.assertEqual(self.run_guard(args, SHIPYARD_INTERNAL_QUEUE_MUTATION="1")[0], 0)
        for ancestry in (["bash", "zsh", "codex"], ["bash", "node", "shipyard"], ["python3"], []):
            with self.subTest(ancestry=ancestry), mock.patch.object(
                guard, "process_ancestry", return_value=ancestry
            ):
                code, message = self.run_guard(args, SHIPYARD_INTERNAL_QUEUE_MUTATION="1")
                self.assertEqual(code, 1)
                self.assertIn("not the Shipyard binary", message)


def pr_answer(number: int = 9706, queued: bool = True) -> dict:
    entry = {"state": "AWAITING_CHECKS", "position": 2} if queued else None
    return {"data": {"node": {"number": number, "headRefOid": "a" * 40, "isInMergeQueue": queued,
                              "mergeQueueEntry": entry, "repository": {"nameWithOwner": "Generous-Corp/pulp"}}}}


def dequeue(node: str = "PR_node9706") -> list[str]:
    return ["api", "graphql", "-f", "query=mutation($id:ID!){dequeuePullRequest(input:{id:$id}){clientMutationId}}",
            "-f", f"id={node}"]


OVERRIDE = {"GHAPP_ALLOW_QUEUE_REMOVAL": "1"}


class OverrideReasonThroughFakeGh(unittest.TestCase):
    """The override is judged by its stated reason, reading the PR through GHAPP_REAL_GH."""

    def run_with_answer(self, args: list[str], answer: dict, **env: str) -> tuple[int, str, list[str], list[dict]]:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            (root / "answer.json").write_text(json.dumps(answer))
            fake = root / "gh"
            fake.write_text(
                "#!/bin/sh\n"
                f"printf '%s\\n' \"$*\" >> '{root}/calls.log'\n"
                f"cat '{root}/answer.json'\n"
            )
            fake.chmod(0o755)
            log = root / "removals.jsonl"
            environment = {"PATH": "/usr/bin:/bin", "GHAPP_REAL_GH": str(fake), "GH_REPO": "Generous-Corp/pulp",
                           "GHAPP_QUEUE_REMOVAL_LOG": str(log), **env}
            result = subprocess.run(
                [sys.executable, str(SCRIPT), *args], env=environment, capture_output=True, text=True
            )
            calls_file = root / "calls.log"
            calls = calls_file.read_text().splitlines() if calls_file.exists() else []
            records = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
            return result.returncode, result.stderr, calls, records

    def test_rebase_is_refused_even_with_the_override(self) -> None:
        code, message, _, records = self.run_with_answer(
            dequeue(), pr_answer(), **OVERRIDE, GHAPP_QUEUE_REMOVAL_REASON="rebase",
            GHAPP_QUEUE_REMOVAL_NOTE="main moved")
        self.assertEqual(code, 1, message)
        self.assertIn("A queued PR does not need a rebase; the queue merges it on top of current main.", message)
        self.assertEqual(records, [])

    def test_reorder_is_refused_with_a_pointer_to_the_jump_enqueue(self) -> None:
        code, message, _, _ = self.run_with_answer(
            dequeue(), pr_answer(), **OVERRIDE, GHAPP_QUEUE_REMOVAL_REASON="reorder",
            GHAPP_QUEUE_REMOVAL_NOTE="let #9800 go first")
        self.assertEqual(code, 1, message)
        self.assertIn("jump", message)

    def test_missing_unknown_or_unexplained_reason_is_refused(self) -> None:
        for env in ({}, {"GHAPP_QUEUE_REMOVAL_REASON": "other", "GHAPP_QUEUE_REMOVAL_NOTE": "x"},
                    {"GHAPP_QUEUE_REMOVAL_REASON": "defect-fix"}):
            with self.subTest(env=env):
                code, message, calls, records = self.run_with_answer(dequeue(), pr_answer(), **OVERRIDE, **env)
                self.assertEqual(code, 1, message)
                self.assertEqual(records, [])
                self.assertEqual(calls, [], "a refused reason reads nothing")

    def test_defect_fix_is_allowed_and_logged(self) -> None:
        code, message, calls, records = self.run_with_answer(
            dequeue(), pr_answer(), **OVERRIDE, GHAPP_QUEUE_REMOVAL_REASON="defect-fix",
            GHAPP_QUEUE_REMOVAL_NOTE="local build shows the cache key ignores the arch")
        self.assertEqual(code, 0, message)
        self.assertIn("WARNING", message)
        self.assertTrue(any("id=PR_node9706" in call for call in calls), calls)
        self.assertEqual(len(records), 1)
        record = records[0]
        self.assertEqual(record["reason"], "defect-fix")
        self.assertEqual(record["note"], "local build shows the cache key ignores the arch")
        self.assertEqual(record["targets"], [{"repo": "Generous-Corp/pulp", "pr": 9706, "head": "a" * 40,
                                              "queue_state": "AWAITING_CHECKS"}])

    def test_disable_auto_by_number_is_logged_against_the_named_pr(self) -> None:
        answer = {"data": {"repository": {"nameWithOwner": "Generous-Corp/pulp",
                                          "pullRequest": pr_answer(9834)["data"]["node"]}}}
        code, message, calls, records = self.run_with_answer(
            ["pr", "merge", "9834", "--disable-auto"], answer, **OVERRIDE,
            GHAPP_QUEUE_REMOVAL_REASON="defect-fix", GHAPP_QUEUE_REMOVAL_NOTE="bug")
        self.assertEqual(code, 0, message)
        self.assertTrue(any("number=9834" in call and "owner=Generous-Corp" in call for call in calls), calls)
        self.assertEqual(records[0]["targets"][0]["pr"], 9834)

    def test_main_red_fix_may_remove_only_the_fix_pr(self) -> None:
        env = {**OVERRIDE, "GHAPP_QUEUE_REMOVAL_REASON": "reorder-main-red-fix",
               "GHAPP_QUEUE_REMOVAL_NOTE": "main red on view-widgets; jump the fix"}
        code, message, _, records = self.run_with_answer(
            dequeue(), pr_answer(9706), **env, GHAPP_QUEUE_REMOVAL_FIX_PR="9706")
        self.assertEqual(code, 0, message)
        self.assertEqual(records[0]["fix_pr"], "9706")
        for fix_pr in ("9800", ""):
            with self.subTest(fix_pr=fix_pr):
                code, message, _, records = self.run_with_answer(
                    dequeue(), pr_answer(9706), **env, GHAPP_QUEUE_REMOVAL_FIX_PR=fix_pr)
                self.assertEqual(code, 1, message)
                self.assertIn("only the PR that fixes main", message)
                self.assertEqual(records, [])

    def test_unreadable_target_refuses_the_override(self) -> None:
        code, message, _, records = self.run_with_answer(
            dequeue(), {"errors": [{"message": "boom"}]}, **OVERRIDE,
            GHAPP_QUEUE_REMOVAL_REASON="defect-fix", GHAPP_QUEUE_REMOVAL_NOTE="bug")
        self.assertEqual(code, 1)
        self.assertIn("cannot read the PR it targets", message)
        self.assertEqual(records, [])

    def test_unwritable_log_refuses_the_removal(self) -> None:
        code, message, _, _ = self.run_with_answer(
            dequeue(), pr_answer(), **OVERRIDE, GHAPP_QUEUE_REMOVAL_REASON="defect-fix",
            GHAPP_QUEUE_REMOVAL_NOTE="bug", GHAPP_QUEUE_REMOVAL_LOG="/dev/null/nope.jsonl")
        self.assertEqual(code, 1)
        self.assertIn("cannot record the removal", message)

    def test_without_the_override_nothing_is_read(self) -> None:
        code, _, calls, _ = self.run_with_answer(
            dequeue(), pr_answer(), GHAPP_QUEUE_REMOVAL_REASON="defect-fix", GHAPP_QUEUE_REMOVAL_NOTE="bug")
        self.assertEqual(code, 1)
        self.assertEqual(calls, [])

    def test_spoofed_marker_from_this_test_process_is_refused(self) -> None:
        code, message, calls, _ = self.run_with_answer(dequeue(), pr_answer(), SHIPYARD_INTERNAL_QUEUE_MUTATION="1")
        self.assertEqual(code, 1)
        self.assertIn("not the Shipyard binary", message)
        self.assertEqual(calls, [])


REASON_FOR_CAUSE = {
    # How each recorded removal states itself under the reason classes.
    "rebase_after_main_moved": "rebase",
    "reorder_by_dequeue": "reorder",
    "defect_fix_push": "defect-fix",
    "receipt_text_correction": "other",
    "update_push_reason_unstated": "",
}


class ReplayClassifiedRemovals(unittest.TestCase):
    """The 46 queue removals agents made on Generous-Corp/pulp, 10-03 to 10-09."""

    FIXTURE = pathlib.Path(__file__).with_name("fixtures") / "queue_removals_classified_2026-10.jsonl"

    def test_avoidable_removals_are_refused_and_defect_fixes_allowed(self) -> None:
        rows = [json.loads(line) for line in self.FIXTURE.read_text().splitlines() if line.strip()]
        self.assertEqual(len(rows), 46)
        decisions: dict[str, list[int]] = {"allowed": [], "refused": []}
        refused_avoidable = 0
        for row in rows:
            reason = REASON_FOR_CAUSE[row["cause"]]
            env = {**OVERRIDE, "GHAPP_QUEUE_REMOVAL_REASON": reason, "GHAPP_QUEUE_REMOVAL_NOTE": row["cause"]}
            with tempfile.TemporaryDirectory() as directory, mock.patch.object(
                guard, "read_target",
                return_value={"repo": "Generous-Corp/pulp", "pr": row["pr"], "head": "a" * 40,
                              "queue_state": "AWAITING_CHECKS"},
            ):
                env["GHAPP_QUEUE_REMOVAL_LOG"] = str(pathlib.Path(directory) / "log.jsonl")
                stderr = io.StringIO()
                with mock.patch.dict(os.environ, env, clear=True), contextlib.redirect_stderr(stderr):
                    code = guard.main(dequeue(f"PR_{row['pr']}"))
            decisions["allowed" if code == 0 else "refused"].append(row["pr"])
            if code != 0 and row["avoidable"] == "yes":
                refused_avoidable += 1
            if row["guard_bypass"].startswith("SHIPYARD_INTERNAL"):
                # The same request as it was actually made: the spoofed marker alone.
                with mock.patch.object(guard, "process_ancestry", return_value=["bash", "codex"]):
                    with mock.patch.dict(os.environ, {"SHIPYARD_INTERNAL_QUEUE_MUTATION": "1"}, clear=True), \
                            contextlib.redirect_stderr(io.StringIO()):
                        self.assertEqual(guard.main(dequeue(f"PR_{row['pr']}")), 1, row)
        defect_fixes = sorted(row["pr"] for row in rows if row["cause"] == "defect_fix_push")
        self.assertEqual(sorted(decisions["allowed"]), defect_fixes)
        self.assertEqual(len(defect_fixes), 7)
        self.assertEqual(refused_avoidable, 36)
        self.assertEqual(len(decisions["refused"]), 39, "36 avoidable + 2 receipt edits + 1 unstated")


class RealProcessAncestry(unittest.TestCase):
    """shipyard_parent() reads the live process tree, not a mock."""

    def run_under(self, launcher_name: str) -> subprocess.CompletedProcess:
        compiler = shutil.which("cc")
        if compiler is None:
            self.skipTest("no C compiler to build a launcher process")
        with tempfile.TemporaryDirectory() as directory:
            source = pathlib.Path(directory) / "launcher.c"
            source.write_text(
                "#include <stdlib.h>\nint main(int c, char **v) { return c > 1 && system(v[1]) == 0 ? 0 : 1; }\n"
            )
            launcher = pathlib.Path(directory) / launcher_name
            built = subprocess.run([compiler, "-o", str(launcher), str(source)], capture_output=True, text=True)
            if built.returncode != 0:
                self.skipTest(f"cannot build a launcher: {built.stderr.strip()}")
            probe = pathlib.Path(directory) / "probe.py"
            probe.write_text(
                "import importlib.util, sys\n"
                f"spec = importlib.util.spec_from_file_location('g', {str(SCRIPT)!r})\n"
                "g = importlib.util.module_from_spec(spec); spec.loader.exec_module(g)\n"
                "print(g.shipyard_parent(), g.process_ancestry())\n"
            )
            # launcher -> system()'s /bin/sh -> bash -> python: the same shape
            # as shipyard -> ghapp (bash) -> guard (python).
            return subprocess.run(
                [str(launcher), f"/bin/bash -c '{sys.executable} {probe}; true'"],
                capture_output=True,
                text=True,
            )

    def test_a_process_named_shipyard_is_recognised(self) -> None:
        result = self.run_under("shipyard")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(result.stdout.startswith("True"), result.stdout + result.stderr)

    def test_any_other_launcher_is_not(self) -> None:
        result = self.run_under("codex")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(result.stdout.startswith("False"), result.stdout + result.stderr)

if __name__ == "__main__":
    unittest.main()

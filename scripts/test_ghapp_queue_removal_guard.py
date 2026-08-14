from __future__ import annotations

import contextlib
import importlib.util
import io
import os
import pathlib
import subprocess
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
        with mock.patch.object(guard.subprocess, "run", return_value=completed):
            self.assertEqual(self.run_guard(["api", "repos/{owner}/{repo}"])[0], 0)

    def test_manual_override_allows_ambiguous_input_loudly(self) -> None:
        code, message = self.run_guard(
            ["api", "graphql", "--input", "-"], GHAPP_ALLOW_QUEUE_REMOVAL="1"
        )
        self.assertEqual(code, 0)
        self.assertIn("WARNING", message)

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

    def test_audited_shipyard_path_and_loud_override_are_allowed(self) -> None:
        args = ["api", "graphql", "-f", "query=mutation { dequeuePullRequest(input:{id:\"x\"}) { clientMutationId } }"]
        self.assertEqual(self.run_guard(args, SHIPYARD_INTERNAL_QUEUE_MUTATION="1")[0], 0)
        code, message = self.run_guard(args, GHAPP_ALLOW_QUEUE_REMOVAL="1")
        self.assertEqual(code, 0)
        self.assertIn("WARNING", message)


if __name__ == "__main__":
    unittest.main()

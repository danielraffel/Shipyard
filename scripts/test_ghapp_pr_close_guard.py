from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import pathlib
import unittest
from unittest import mock


SCRIPT = pathlib.Path(__file__).with_name("ghapp_pr_close_guard.py")
SPEC = importlib.util.spec_from_file_location("ghapp_pr_close_guard", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)


class PrCloseGuardTests(unittest.TestCase):
    def run_guard(
        self,
        args: list[str],
        responses: dict[str, dict[str, object]] | None = None,
        **env: str,
    ) -> tuple[int, str, list[str]]:
        stderr = io.StringIO()
        endpoints: list[str] = []

        def api_json(endpoint: str) -> dict[str, object]:
            endpoints.append(endpoint)
            if responses is None or endpoint not in responses:
                raise guard.GuardError(f"unexpected endpoint: {endpoint}")
            return responses[endpoint]

        with (
            mock.patch.dict(os.environ, env, clear=True),
            contextlib.redirect_stderr(stderr),
        ):
            return guard.main(args, api_json=api_json), stderr.getvalue(), endpoints

    def test_harmless_commands_are_allowed_without_network(self) -> None:
        self.assertEqual(self.run_guard(["pr", "view", "7476"])[0], 0)
        self.assertEqual(self.run_guard(["api", "repos/o/r/pulls/7476"])[0], 0)

    def test_pr_close_accepts_flags_before_selector(self) -> None:
        self.assertEqual(
            guard.close_request(["pr", "close", "-R", "o/r", "--delete-branch", "7"]),
            guard.CloseRequest(repo="o/r", pr=7),
        )

    def test_compare_relation_contract(self) -> None:
        cases = {
            "ahead": ({"status": "ahead", "ahead_by": 3, "behind_by": 0}, False),
            "behind": ({"status": "behind", "ahead_by": 0, "behind_by": 3}, True),
            "identical": ({"status": "identical", "ahead_by": 0, "behind_by": 0}, True),
            "diverged": ({"status": "diverged", "ahead_by": 2, "behind_by": 4}, False),
        }
        for name, (comparison, expected) in cases.items():
            with self.subTest(name=name):
                self.assertEqual(guard.head_is_contained(comparison), expected)

    def test_exact_7476_ahead_tuple_is_not_integrated(self) -> None:
        responses = {
            "repos/Generous-Corp/pulp/pulls/7476": {
                "head": {"sha": "6c7ece533b36775055860b55d0cc45e3f8e3962c"},
                "base": {"ref": "main"},
            },
            "repos/Generous-Corp/pulp/commits/main": {
                "sha": "aa630815630ca259b743651dacdc335f3d94a39a"
            },
            "repos/Generous-Corp/pulp/compare/aa630815630ca259b743651dacdc335f3d94a39a...6c7ece533b36775055860b55d0cc45e3f8e3962c": {
                "status": "ahead",
                "ahead_by": 3,
                "behind_by": 0,
            },
        }
        code, message, endpoints = self.run_guard(
            ["pr", "close", "7476", "--repo", "Generous-Corp/pulp"], responses
        )
        self.assertEqual(code, 1)
        self.assertIn("3 unique commit(s)", message)
        self.assertEqual(
            endpoints[-1],
            "repos/Generous-Corp/pulp/compare/aa630815630ca259b743651dacdc335f3d94a39a...6c7ece533b36775055860b55d0cc45e3f8e3962c",
        )

    def test_behind_and_identical_heads_may_close_as_integrated(self) -> None:
        for status, behind_by in (("behind", 3), ("identical", 0)):
            with self.subTest(status=status):
                responses = {
                    "repos/o/r/pulls/7": {
                        "head": {"sha": "h" * 40},
                        "base": {"ref": "main"},
                    },
                    "repos/o/r/commits/main": {"sha": "b" * 40},
                    f"repos/o/r/compare/{'b' * 40}...{'h' * 40}": {
                        "status": status,
                        "ahead_by": 0,
                        "behind_by": behind_by,
                    },
                }
                self.assertEqual(
                    self.run_guard(["pr", "close", "7", "-R", "o/r"], responses)[0],
                    0,
                )

    def test_diverged_head_is_refused(self) -> None:
        responses = {
            "repos/o/r/pulls/7": {
                "head": {"sha": "h" * 40},
                "base": {"ref": "main"},
            },
            "repos/o/r/commits/main": {"sha": "b" * 40},
            f"repos/o/r/compare/{'b' * 40}...{'h' * 40}": {
                "status": "diverged",
                "ahead_by": 2,
                "behind_by": 4,
            },
        }
        code, message, _ = self.run_guard(["pr", "close", "7", "-R", "o/r"], responses)
        self.assertEqual(code, 1)
        self.assertIn("diverged", message)

    def test_diverged_history_may_close_only_when_changed_content_matches(self) -> None:
        base = "b" * 40
        head = "h" * 40
        blob = "c" * 40
        responses = {
            "repos/o/r/pulls/7": {
                "head": {"sha": head},
                "base": {"ref": "main"},
            },
            "repos/o/r/commits/main": {"sha": base},
            f"repos/o/r/compare/{base}...{head}": {
                "status": "diverged",
                "ahead_by": 2,
                "behind_by": 4,
                "files": [
                    {"status": "modified", "filename": "src/lib.rs", "sha": blob}
                ],
            },
            f"repos/o/r/contents/src/lib.rs?ref={base}": {"sha": blob},
        }
        self.assertEqual(
            self.run_guard(["pr", "close", "7", "-R", "o/r"], responses)[0],
            0,
        )
        responses[f"repos/o/r/contents/src/lib.rs?ref={base}"] = {"sha": "d" * 40}
        self.assertEqual(
            self.run_guard(["pr", "close", "7", "-R", "o/r"], responses)[0],
            1,
        )

    def test_truncated_file_evidence_cannot_prove_content_containment(self) -> None:
        files = [
            {"status": "modified", "filename": f"src/{index}.rs", "sha": "c" * 40}
            for index in range(300)
        ]
        self.assertFalse(
            guard.changed_content_is_contained(
                "o/r",
                "b" * 40,
                {"files": files},
                lambda endpoint: self.fail(f"unexpected query: {endpoint}"),
            )
        )

    def test_raw_rest_close_is_recognized(self) -> None:
        self.assertEqual(
            guard.close_request(
                ["api", "-X", "PATCH", "repos/o/r/pulls/7", "-f", "state=closed"]
            ),
            guard.CloseRequest(repo="o/r", pr=7),
        )

    def test_rest_endpoint_accepts_leading_slash_and_query(self) -> None:
        self.assertEqual(
            guard.close_request(
                ["api", "-X", "PATCH", "/repos/o/r/pulls/7?apiVersion=2022", "-f", "state=closed"]
            ),
            guard.CloseRequest(repo="o/r", pr=7),
        )

    def test_rest_state_in_endpoint_query_is_recognized(self) -> None:
        self.assertEqual(
            guard.close_request(
                ["api", "-XPATCH", "repos/o/r/pulls/7?state=%63losed"]
            ),
            guard.CloseRequest(repo="o/r", pr=7),
        )

    def test_attached_short_options_are_recognized(self) -> None:
        self.assertEqual(
            guard.close_request(
                ["api", "-XPATCH", "repos/o/r/pulls/7", "-fstate=closed"]
            ),
            guard.CloseRequest(repo="o/r", pr=7),
        )

    def test_flag_value_that_looks_like_endpoint_cannot_hide_real_endpoint(self) -> None:
        self.assertEqual(
            guard.close_request(
                [
                    "api",
                    "-q",
                    "graphql",
                    "repos/o/r/pulls/7",
                    "-XPATCH",
                    "-fstate=closed",
                ]
            ),
            guard.CloseRequest(repo="o/r", pr=7),
        )

    def test_typed_state_file_close_is_recognized_and_stdin_fails_closed(self) -> None:
        with mock.patch("pathlib.Path.read_text", return_value="closed"):
            self.assertEqual(
                guard.close_request(
                    ["api", "repos/o/r/pulls/7", "-XPATCH", "-Fstate=@/tmp/state"]
                ),
                guard.CloseRequest(repo="o/r", pr=7),
            )
        code, message, _ = self.run_guard(
            ["api", "repos/o/r/pulls/7", "-XPATCH", "-Fstate=@-"]
        )
        self.assertEqual(code, 1)
        self.assertIn("stdin", message)
        self.assertEqual(
            guard.close_request(["pr", "close", "7", "-Ro/r"]),
            guard.CloseRequest(repo="o/r", pr=7),
        )

    def test_raw_rest_input_file_close_is_recognized(self) -> None:
        with mock.patch("pathlib.Path.read_text", return_value='{"state":"closed"}'):
            self.assertEqual(
                guard.close_request(
                    [
                        "api",
                        "repos/o/r/pulls/7",
                        "--method",
                        "PATCH",
                        "--input",
                        "/tmp/request.json",
                    ]
                ),
                guard.CloseRequest(repo="o/r", pr=7),
            )

    def test_uninspectable_stdin_patch_fails_closed(self) -> None:
        code, message, _ = self.run_guard(
            [
                "api",
                "repos/o/r/pulls/7",
                "--method",
                "PATCH",
                "--input",
                "-",
            ]
        )
        self.assertEqual(code, 1)
        self.assertIn("stdin", message)

    def test_raw_graphql_close_is_refused_without_bypass(self) -> None:
        code, message, _ = self.run_guard(
            [
                "api",
                "graphql",
                "-f",
                "query=mutation { closePullRequest(input:{pullRequestId:\"x\"}) { clientMutationId } }",
            ]
        )
        self.assertEqual(code, 1)
        self.assertIn("closePullRequest", message)

    def test_graphql_endpoint_is_recognized_after_flags(self) -> None:
        code, message, _ = self.run_guard(
            [
                "api",
                "-XPOST",
                "graphql",
                "-fquery=mutation { closePullRequest(input:{pullRequestId:\"x\"}) { clientMutationId } }",
            ]
        )
        self.assertEqual(code, 1)
        self.assertIn("closePullRequest", message)

    def test_graphql_mutation_in_endpoint_query_is_refused(self) -> None:
        code, message, _ = self.run_guard(
            [
                "api",
                "-XPOST",
                "graphql?query=mutation%20%7BclosePullRequest(input:%7BpullRequestId:%22x%22%7D)%7BclientMutationId%7D%7D",
            ]
        )
        self.assertEqual(code, 1)
        self.assertIn("closePullRequest", message)

    def test_graphql_stdin_body_fails_closed(self) -> None:
        for args in (
            ["api", "graphql", "--input", "-"],
            ["api", "graphql", "-fquery=@-"],
        ):
            with self.subTest(args=args):
                code, message, _ = self.run_guard(args)
                self.assertEqual(code, 1)
                self.assertIn("stdin", message)

    def test_raw_graphql_input_file_close_is_refused(self) -> None:
        mutation = (
            '{"query":"mutation { closePullRequest(input:{pullRequestId:\\\"x\\\"}) '
            '{ clientMutationId } }"}'
        )
        with mock.patch("pathlib.Path.read_text", return_value=mutation):
            code, message, _ = self.run_guard(
                ["api", "graphql", "--input", "/tmp/request.json"]
            )
        self.assertEqual(code, 1)
        self.assertIn("closePullRequest", message)

    def test_rest_issue_close_is_allowed_only_when_number_is_not_a_pr(self) -> None:
        def issue_only(endpoint: str) -> dict[str, object]:
            if endpoint == "repos/o/r/pulls/7":
                raise guard.NotFound(f"missing {endpoint}")
            if endpoint == "repos/o/r/issues/7":
                return {"number": 7}
            self.fail(f"unexpected endpoint: {endpoint}")

        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            code = guard.main(
                ["api", "-X", "PATCH", "repos/o/r/issues/7", "-f", "state=closed"],
                api_json=issue_only,
            )
        self.assertEqual(code, 0)

    def test_issue_alias_fails_closed_when_pull_read_is_ambiguous(self) -> None:
        def ambiguous(endpoint: str) -> dict[str, object]:
            if endpoint == "repos/o/r/pulls/7":
                raise guard.NotFound(f"missing {endpoint}")
            if endpoint == "repos/o/r/issues/7":
                return {"number": 7, "pull_request": {"url": "https://api.github.test/pulls/7"}}
            self.fail(f"unexpected endpoint: {endpoint}")

        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            code = guard.main(
                ["issue", "close", "7", "-R", "o/r"],
                api_json=ambiguous,
            )
        self.assertEqual(code, 1)
        self.assertIn("could not be inspected", stderr.getvalue())

    def test_high_level_issue_close_is_checked_for_pr_aliasing(self) -> None:
        self.assertEqual(
            guard.close_request(["issue", "close", "7", "-R", "o/r"]),
            guard.CloseRequest(repo="o/r", pr=7, allow_non_pr=True),
        )

    def test_malformed_or_unreadable_evidence_fails_closed(self) -> None:
        code, message, _ = self.run_guard(["pr", "close", "7", "-R", "o/r"], {})
        self.assertEqual(code, 1)
        self.assertIn("could not prove", message)

    def test_loud_override_allows_deliberate_nonintegrated_close(self) -> None:
        code, message, endpoints = self.run_guard(
            ["pr", "close", "7", "-R", "o/r"],
            {},
            GHAPP_ALLOW_UNINTEGRATED_PR_CLOSE="1",
        )
        self.assertEqual(code, 0)
        self.assertIn("WARNING", message)
        self.assertEqual(endpoints, [])


if __name__ == "__main__":
    unittest.main()

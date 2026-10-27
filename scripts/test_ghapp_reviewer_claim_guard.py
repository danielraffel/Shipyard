"""Focused tests for the durable reviewer-claim ghapp gate."""

from __future__ import annotations

import importlib.util
import json
import os
import pathlib
import stat
import tempfile
import unittest
from unittest import mock


ROOT = pathlib.Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("reviewer_claim_guard", ROOT / "ghapp_reviewer_claim_guard.py")
assert SPEC and SPEC.loader
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)

HEAD = "a" * 40


class FakePolicyGuard:
    def __init__(self, enabled: bool) -> None:
        self.enabled = enabled

    def base_config_text(self, _repo: str, _base: str) -> str | None:
        return "base"

    def review_policy(self, _config: str | None, _repo: str, _default: bool) -> dict[str, object]:
        return {
            "claim_receipt": "shipyard-reviewer-claim" if self.enabled else None,
            "claim_app": 15368 if self.enabled else None,
        }

    def reviewer_claim_receipt(self, _repo: str, _pr: int, _head: str,
                               _policy: dict[str, object], approval: dict[str, object]) -> str:
        if approval.get("signer") != "reviewer-a":
            raise RuntimeError("receipt reviewer mismatch")
        return "ok"


class ReviewerClaimGuard(unittest.TestCase):
    def test_non_verdict_requests_are_inert(self) -> None:
        self.assertEqual(guard.main(["pr", "comment", "42", "--body", "status only"]), 0)

    def test_body_files_and_api_fields_are_inspected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "body.txt"
            path.write_text(f"reviewed: {HEAD}\n", encoding="utf-8")
            values = guard._body_values(["pr", "comment", "42", "--body-file", str(path)])
            self.assertIn(f"reviewed: {HEAD}\n", values)
        self.assertEqual(
            guard._target(["api", f"repos/o/r/issues/42/comments", "-f", f"body=verdict: APPROVE head:{HEAD}"], "o/r"),
            (42, "o/r"),
        )
        self.assertEqual(
            guard._target(["pr", "review", f"https://github.com/o/r/pull/42"], "o/r"),
            (42, "o/r"),
        )
        self.assertEqual(
            guard._target(["api", f"repos/o/r/pulls/42/reviews"], "o/r"),
            (42, "o/r"),
        )

    def test_typed_api_body_file_is_inspected_without_confusing_dash_f(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "body.txt"
            path.write_text(f"verdict: HOLD head:{HEAD}\n", encoding="utf-8")
            self.assertIn(
                f"verdict: HOLD head:{HEAD}\n",
                guard._body_values([
                    "api", "repos/o/r/issues/42/comments", "-F", f"body=@{path}",
                ]),
            )
            self.assertIn(
                f"verdict: HOLD head:{HEAD}\n",
                guard._body_values(["pr", "comment", "42", "-F", str(path)]),
            )

    def test_body_file_sources_fail_closed_when_not_inspectable(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            missing = pathlib.Path(directory) / "missing"
            fifo = pathlib.Path(directory) / "pipe"
            unreadable = pathlib.Path(directory) / "unreadable"
            unreadable.write_text("verdict: APPROVE head:" + HEAD, encoding="utf-8")
            unreadable.chmod(0)
            os.mkfifo(fifo)
            for option, path in (("--body-file", "-"), ("--comment-file", "-"),
                                 ("--body-file", str(missing)),
                                 ("--comment-file", str(missing)),
                                 ("--body-file", str(fifo)),
                                 ("--body-file", str(unreadable))):
                with self.subTest(option=option, path=path):
                    self.assertTrue(guard._opaque_body_source(
                        ["pr", "comment", "42", option, path]))
                    self.assertEqual(guard.main(
                        ["pr", "comment", "42", option, path]), 1)

    def test_api_stdin_and_pipe_field_sources_fail_closed(self) -> None:
        for args in (
            ["api", "repos/o/r/issues/42/comments", "--input", "-"],
            ["api", "repos/o/r/issues/42/comments", "--input", "/dev/stdin"],
            ["api", "repos/o/r/issues/42/comments", "-f", "body=@/dev/stdin"],
            ["api", "graphql", "-f", "query=@/dev/stdin"],
        ):
            with self.subTest(args=args):
                self.assertTrue(guard._opaque_body_source(args))
                self.assertEqual(guard.main(args), 1)

    def test_comment_file_is_read_and_verdict_is_not_inert(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "comment.md"
            path.write_text(f"verdict: HOLD head:{HEAD}\n", encoding="utf-8")
            args = ["pr", "comment", "42", "--comment-file", str(path)]
            self.assertFalse(guard._opaque_body_source(args))
            self.assertIn(f"verdict: HOLD head:{HEAD}\n", guard._body_values(args))

    def test_default_off_policy_does_not_need_a_local_claim(self) -> None:
        with mock.patch.object(guard, "_live_pull", return_value=(HEAD, "main")), \
             mock.patch.object(guard, "_guard_module", return_value=FakePolicyGuard(False)), \
             mock.patch.dict(os.environ, {"GH_REPO": "o/r"}, clear=False):
            self.assertEqual(
                guard.main(["pr", "comment", "42", "--body", f"verdict: APPROVE head:{HEAD}"]), 0
            )

    def test_enabled_policy_calls_shipyard_validate(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            binary = pathlib.Path(directory) / "shipyard"
            binary.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
            env = {
                "GH_REPO": "o/r",
                "SHIPYARD_AGENT_ID": "reviewer-a",
                "SHIPYARD_GHAPP_SHIPYARD_BINARY": str(binary),
            }
            with mock.patch.object(guard, "_live_pull", return_value=(HEAD, "main")), \
                 mock.patch.object(guard, "_guard_module", return_value=FakePolicyGuard(True)), \
                 mock.patch.dict(os.environ, env, clear=False), \
                 mock.patch.object(guard.subprocess, "run", wraps=guard.subprocess.run) as run:
                self.assertEqual(
                    guard.main(["pr", "comment", "42", "--body", f"verdict: HOLD head:{HEAD}"]), 0
                )
                command = next(call.args[0] for call in run.call_args_list if call.args[0][0] == str(binary))
                self.assertEqual(command[-1], "reviewer-a")

    def test_enabled_policy_refuses_when_validate_is_stale(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            binary = pathlib.Path(directory) / "shipyard"
            binary.write_text("#!/bin/sh\nexit 1\n", encoding="utf-8")
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
            with mock.patch.object(guard, "_live_pull", return_value=(HEAD, "main")), \
                 mock.patch.object(guard, "_guard_module", return_value=FakePolicyGuard(True)), \
                 mock.patch.dict(os.environ, {
                     "GH_REPO": "o/r",
                     "SHIPYARD_AGENT_ID": "reviewer-a",
                     "SHIPYARD_GHAPP_SHIPYARD_BINARY": str(binary),
                 }, clear=False):
                self.assertEqual(
                    guard.main(["pr", "comment", "42", "--body", f"verdict: APPROVE head:{HEAD}"]), 1
                )

    def test_enabled_policy_requires_immutable_app_id(self) -> None:
        policy = FakePolicyGuard(True)
        policy.review_policy = lambda *_args: {
            "claim_receipt": "shipyard-reviewer-claim", "claim_app": "mutable-slug"
        }
        with tempfile.TemporaryDirectory() as directory:
            binary = pathlib.Path(directory) / "shipyard"
            binary.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
            with mock.patch.object(guard, "_live_pull", return_value=(HEAD, "main")), \
                 mock.patch.object(guard, "_guard_module", return_value=policy), \
                 mock.patch.dict(os.environ, {
                     "GH_REPO": "o/r", "SHIPYARD_AGENT_ID": "reviewer-a",
                     "SHIPYARD_GHAPP_SHIPYARD_BINARY": str(binary),
                 }, clear=False):
                self.assertEqual(guard.main([
                    "pr", "comment", "42", "--body", f"verdict: APPROVE head:{HEAD}"
                ]), 1)

    def test_enabled_policy_gates_bodyless_review_actions(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            binary = pathlib.Path(directory) / "shipyard"
            binary.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
            with mock.patch.object(guard, "_live_pull", return_value=(HEAD, "main")), \
                 mock.patch.object(guard, "_guard_module", return_value=FakePolicyGuard(True)), \
                 mock.patch.object(guard.subprocess, "run", wraps=guard.subprocess.run) as run, \
                 mock.patch.dict(os.environ, {
                     "GH_REPO": "o/r",
                     "SHIPYARD_AGENT_ID": "reviewer-a",
                     "SHIPYARD_GHAPP_SHIPYARD_BINARY": str(binary),
                 }, clear=False):
                self.assertEqual(guard.main(["pr", "review", "42", "--approve"]), 0)
                command = next(call.args[0] for call in run.call_args_list if call.args[0][0] == str(binary))
                self.assertIn("--head", command)

    def test_enabled_policy_gates_bodyless_rest_review_actions(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            binary = pathlib.Path(directory) / "shipyard"
            binary.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
            with mock.patch.object(guard, "_live_pull", return_value=(HEAD, "main")), \
                 mock.patch.object(guard, "_guard_module", return_value=FakePolicyGuard(True)), \
                 mock.patch.dict(os.environ, {
                     "GH_REPO": "o/r",
                     "SHIPYARD_AGENT_ID": "reviewer-a",
                     "SHIPYARD_GHAPP_SHIPYARD_BINARY": str(binary),
                 }, clear=False):
                self.assertEqual(guard.main([
                    "api", "repos/o/r/pulls/42/reviews", "-f", "event=APPROVE",
                ]), 0)

    def test_input_equals_rest_event_is_gated(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "review.json"
            path.write_text(json.dumps({"event": "APPROVE", "commit_id": HEAD}), encoding="utf-8")
            binary = pathlib.Path(directory) / "shipyard"
            binary.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
            with mock.patch.object(guard, "_live_pull", return_value=(HEAD, "main")), \
                 mock.patch.object(guard, "_guard_module", return_value=FakePolicyGuard(True)), \
                 mock.patch.dict(os.environ, {
                     "GH_REPO": "o/r",
                     "SHIPYARD_AGENT_ID": "reviewer-a",
                     "SHIPYARD_GHAPP_SHIPYARD_BINARY": str(binary),
                 }, clear=False), \
                 mock.patch.object(guard.subprocess, "run", wraps=guard.subprocess.run) as run:
                self.assertEqual(guard.main([
                    "api", "repos/o/r/pulls/42/reviews", f"--input={path}",
                ]), 0)
                self.assertTrue(any(call.args[0][0] == str(binary) for call in run.call_args_list))

    def test_graphql_input_variables_body_is_gated(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "graphql.json"
            path.write_text(json.dumps({
                "query": "mutation { addComment(input: {subjectId: $id, body: $body}) { clientMutationId } }",
                "variables": {"id": "PR_kwDO123", "body": f"verdict: APPROVE head: {HEAD}"},
            }), encoding="utf-8")
            binary = pathlib.Path(directory) / "shipyard"
            binary.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
            with mock.patch.object(guard, "_target", return_value=(42, "o/r")), \
                 mock.patch.object(guard, "_live_pull", return_value=(HEAD, "main")), \
                 mock.patch.object(guard, "_guard_module", return_value=FakePolicyGuard(True)), \
                 mock.patch.dict(os.environ, {
                     "GH_REPO": "o/r",
                     "SHIPYARD_AGENT_ID": "reviewer-a",
                     "SHIPYARD_GHAPP_SHIPYARD_BINARY": str(binary),
                 }, clear=False), \
                 mock.patch.object(guard.subprocess, "run", wraps=guard.subprocess.run) as run:
                self.assertEqual(guard.main(["api", "graphql", f"--input={path}" ]), 0)
                self.assertTrue(any(call.args[0][0] == str(binary) for call in run.call_args_list))

    def test_graphql_query_file_body_is_gated(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            query = pathlib.Path(directory) / "query.graphql"
            query.write_text(
                f'mutation {{ addComment(input: {{subjectId: $id, body: "verdict: APPROVE head: {HEAD}"}}) {{ clientMutationId }} }}',
                encoding="utf-8",
            )
            binary = pathlib.Path(directory) / "shipyard"
            binary.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
            with mock.patch.object(guard, "_target", return_value=(42, "o/r")), \
                 mock.patch.object(guard, "_live_pull", return_value=(HEAD, "main")), \
                 mock.patch.object(guard, "_guard_module", return_value=FakePolicyGuard(True)), \
                 mock.patch.dict(os.environ, {
                     "GH_REPO": "o/r",
                     "SHIPYARD_AGENT_ID": "reviewer-a",
                     "SHIPYARD_GHAPP_SHIPYARD_BINARY": str(binary),
                 }, clear=False), \
                 mock.patch.object(guard.subprocess, "run", wraps=guard.subprocess.run) as run:
                self.assertEqual(guard.main([
                    "api", "graphql", "-f", f"query=@{query}", "-f", "id=PR_kwDO123",
                ]), 0)
                self.assertTrue(any(call.args[0][0] == str(binary) for call in run.call_args_list))

    def test_enabled_policy_gates_rest_review_input_event(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            binary = pathlib.Path(directory) / "shipyard"
            binary.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
            payload = pathlib.Path(directory) / "review.json"
            payload.write_text('{"event":"APPROVE"}', encoding="utf-8")
            with mock.patch.object(guard, "_live_pull", return_value=(HEAD, "main")), \
                 mock.patch.object(guard, "_guard_module", return_value=FakePolicyGuard(True)), \
                 mock.patch.dict(os.environ, {
                     "GH_REPO": "o/r",
                     "SHIPYARD_AGENT_ID": "reviewer-a",
                     "SHIPYARD_GHAPP_SHIPYARD_BINARY": str(binary),
                 }, clear=False):
                self.assertEqual(guard.main([
                    "api", "repos/o/r/pulls/42/reviews", "--input", str(payload),
                ]), 0)

    def test_enabled_policy_gates_graphql_input_body(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            binary = pathlib.Path(directory) / "shipyard"
            binary.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
            payload = pathlib.Path(directory) / "graphql.json"
            payload.write_text(
                '{"query":"mutation { addComment(input: {subjectId: $id, body: $body}) { clientMutationId } }",'
                '"variables":{"id":"PR_kwDO123","body":"verdict: APPROVE head:' + HEAD + '"}}',
                encoding="utf-8",
            )
            graphql = mock.Mock(
                returncode=0,
                stdout='{"data":{"node":{"number":42,"repository":{"nameWithOwner":"o/r"}}}}',
                stderr="",
            )
            with mock.patch.object(guard, "_live_pull", return_value=(HEAD, "main")), \
                 mock.patch.object(guard, "_guard_module", return_value=FakePolicyGuard(True)), \
                 mock.patch.object(guard.subprocess, "run", side_effect=[graphql, mock.Mock(returncode=0)]), \
                 mock.patch.dict(os.environ, {
                     "GH_REPO": "o/r",
                     "SHIPYARD_AGENT_ID": "reviewer-a",
                     "SHIPYARD_GHAPP_SHIPYARD_BINARY": str(binary),
                 }, clear=False):
                self.assertEqual(guard.main(["api", "graphql", "--input", str(payload)]), 0)

    def test_graphql_subject_id_resolves_to_pull_request(self) -> None:
        result = mock.Mock(returncode=0, stdout='{"data":{"node":{"number":42,"repository":{"nameWithOwner":"o/r"}}}}', stderr="")
        with mock.patch.object(guard.subprocess, "run", return_value=result):
            self.assertEqual(
                guard._target([
                    "api", "graphql", "-f", "query=mutation { addComment(input: {subjectId: $id}) { clientMutationId } }",
                    "-f", "subjectId=PR_kwDO123",
                ], "o/r"),
                (42, "o/r"),
            )

    def test_live_head_mismatch_refuses(self) -> None:
        with mock.patch.object(guard, "_live_pull", return_value=("b" * 40, "main")), \
             mock.patch.dict(os.environ, {"GH_REPO": "o/r"}, clear=False):
            self.assertEqual(
                guard.main(["pr", "comment", "42", "--body", f"verdict: APPROVE head:{HEAD}"]), 1
            )


if __name__ == "__main__":
    unittest.main()

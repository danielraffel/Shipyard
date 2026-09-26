from __future__ import annotations

import contextlib
import importlib.machinery
import importlib.util
import io
import json
import os
import pathlib
import sys
import tempfile
import unittest
from typing import Any
from unittest import mock


SCRIPT = pathlib.Path(__file__).with_name("ghapp_branch_refresh_guard.py")
SPEC = importlib.util.spec_from_file_location("ghapp_branch_refresh_guard", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)

ONLY = guard.POLICY_ONLY_IF_CONFLICTING


def check_run(name: str, conclusion: str | None, required: bool = True) -> dict[str, Any]:
    return {
        "__typename": "CheckRun",
        "name": name,
        "conclusion": conclusion,
        "status": "COMPLETED" if conclusion else "IN_PROGRESS",
        "isRequired": required,
    }


def pr_response(
    *,
    mergeable: str = "MERGEABLE",
    merge_state: str = "BEHIND",
    queue: bool = True,
    state: str = "OPEN",
    contexts: list[dict[str, Any]] | None = None,
    has_next_page: bool = False,
    in_queue: bool = False,
) -> dict[str, Any]:
    """Shape of a live ``PR_QUERY`` response (captured from Generous-Corp/pulp#8885)."""
    if contexts is None:
        contexts = [check_run("macos", None), check_run("Enforce version & skill sync", "SUCCESS")]
    return {
        "data": {
            "repository": {
                "mergeQueue": {"id": "MQ_kwDOexample"} if queue else None,
                "pullRequest": {
                    "number": 8885,
                    "state": state,
                    "baseRefName": "main",
                    "headRefOid": "e1ae49076" + "0" * 31,
                    "mergeable": mergeable,
                    "mergeStateStatus": merge_state,
                    "isInMergeQueue": in_queue,
                    "commits": {
                        "nodes": [
                            {
                                "commit": {
                                    "statusCheckRollup": {
                                        "contexts": {
                                            "pageInfo": {"hasNextPage": has_next_page},
                                            "nodes": contexts,
                                        }
                                    }
                                }
                            }
                        ]
                    },
                },
            }
        }
    }


def decide(response: dict[str, Any], policy: str = ONLY) -> bool:
    allowed, _ = guard.decide(policy, guard.refresh_facts(response))
    return allowed


class PolicyParsing(unittest.TestCase):
    CONFIG = (
        '[project]\nname = "pulp"\n\n[merge]\nrequire_platforms = ["macos"]\n'
        'refresh_branch = "only-if-conflicting"  # queue validates the merge\n\n'
        '[merge_steward]\nrefresh_branch = "always"\n'
    )

    def both_parsers(self, text: str) -> list[str | None]:
        results = [guard.policy_from_toml(text)]
        with mock.patch.dict(sys.modules, {"tomllib": None}):
            results.append(guard.policy_from_toml(text))
        return results

    def test_reads_only_the_merge_table(self) -> None:
        self.assertEqual(self.both_parsers(self.CONFIG), [ONLY, ONLY])

    def test_absent_key_or_table_is_unset(self) -> None:
        self.assertEqual(self.both_parsers('[merge]\nrequire_platforms = ["macos"]\n'), [None, None])
        self.assertEqual(self.both_parsers('[merge_steward]\nrefresh_branch = "x"\n'), [None, None])

    def test_non_string_value_is_an_error(self) -> None:
        with self.assertRaises(guard.GuardError):
            guard.policy_from_toml("[merge]\nrefresh_branch = true\n")
        with mock.patch.dict(sys.modules, {"tomllib": None}):
            with self.assertRaises(guard.GuardError):
                guard.policy_from_toml("[merge]\nrefresh_branch = true\n")

    def test_unset_and_unknown_values_mean_always(self) -> None:
        self.assertEqual(guard.normalize_policy(None), ("always", None))
        self.assertEqual(guard.normalize_policy("always"), ("always", None))
        self.assertEqual(guard.normalize_policy(ONLY), (ONLY, None))
        policy, warning = guard.normalize_policy("never")
        self.assertEqual(policy, "always")
        self.assertIn("unrecognized", warning or "")


class Decisions(unittest.TestCase):
    def test_default_policy_allows_every_refresh(self) -> None:
        # Control for every refusal below: the identical state is allowed
        # under the default, so the default changes nothing.
        self.assertTrue(decide(pr_response(), policy="always"))

    def test_mergeable_pr_with_gate_in_flight_under_a_queue_is_refused(self) -> None:
        facts = guard.refresh_facts(pr_response())
        self.assertEqual(facts["failing_required"], [])
        self.assertIs(facts["conflicting"], False)
        allowed, reason = guard.decide(ONLY, facts)
        self.assertFalse(allowed)
        self.assertIn("merge queue", reason)
        self.assertIn("shipyard ship --pr 8885", reason)
        self.assertNotIn("GHAPP_ALLOW", reason)

    def test_green_and_queued_prs_are_refused_too(self) -> None:
        green = [check_run("macos", "SUCCESS")]
        self.assertFalse(decide(pr_response(contexts=green, merge_state="CLEAN")))
        self.assertFalse(decide(pr_response(contexts=green, in_queue=True)))

    def test_conflicting_pr_is_always_refreshable(self) -> None:
        self.assertTrue(decide(pr_response(mergeable="CONFLICTING", merge_state="DIRTY")))
        self.assertTrue(decide(pr_response(mergeable="UNKNOWN", merge_state="DIRTY")))

    def test_failing_required_check_is_refreshable(self) -> None:
        for conclusion in ("FAILURE", "TIMED_OUT", "CANCELLED", "STARTUP_FAILURE"):
            with self.subTest(conclusion=conclusion):
                self.assertTrue(decide(pr_response(contexts=[check_run("macos", conclusion)])))
        status = {"__typename": "StatusContext", "context": "ci", "state": "ERROR", "isRequired": True}
        self.assertTrue(decide(pr_response(contexts=[status])))

    def test_failing_advisory_check_does_not_license_a_refresh(self) -> None:
        contexts = [check_run("linux", "FAILURE", required=False), check_run("macos", None)]
        self.assertFalse(decide(pr_response(contexts=contexts)))

    def test_base_without_a_merge_queue_is_refreshable(self) -> None:
        self.assertTrue(decide(pr_response(queue=False)))

    def test_anything_unreadable_is_allowed(self) -> None:
        self.assertTrue(decide(pr_response(mergeable="UNKNOWN", merge_state="UNKNOWN")))
        self.assertTrue(decide(pr_response(has_next_page=True)))
        self.assertTrue(decide(pr_response(state="CLOSED")))
        self.assertTrue(decide({"errors": [{"message": "boom"}]}))
        missing_queue = pr_response()
        del missing_queue["data"]["repository"]["mergeQueue"]
        self.assertTrue(decide(missing_queue))


class RequestDetection(unittest.TestCase):
    def test_pr_update_branch_forms(self) -> None:
        self.assertEqual(
            guard.refresh_request(["pr", "update-branch", "12", "--repo", "o/r"]), [("o", "r", 12)]
        )
        self.assertEqual(
            guard.refresh_request(["pr", "update-branch", "--rebase", "https://github.com/o/r/pull/9"]),
            [("o", "r", 9)],
        )
        with mock.patch.dict(os.environ, {"GH_REPO": "Generous-Corp/pulp"}):
            self.assertEqual(
                guard.refresh_request(["pr", "update-branch", "#7"]), [("Generous-Corp", "pulp", 7)]
            )

    def test_rest_update_branch(self) -> None:
        self.assertEqual(
            guard.refresh_request(["api", "-X", "PUT", "repos/o/r/pulls/7/update-branch"]),
            [("o", "r", 7)],
        )

    def test_graphql_update_pull_request_branch(self) -> None:
        node = {"data": {"node": {"number": 5, "repository": {"nameWithOwner": "o/r"}}}}
        with mock.patch.object(guard, "run_real_gh", return_value=node) as reader:
            targets = guard.refresh_request(
                [
                    "api",
                    "graphql",
                    "-f",
                    "query=mutation($id:ID!){updatePullRequestBranch(input:{pullRequestId:$id}){clientMutationId}}",
                    "-f",
                    "id=PR_kwDOabc",
                ]
            )
        self.assertEqual(targets, [("o", "r", 5)])
        self.assertIn("id=PR_kwDOabc", reader.call_args.args[0])

    def test_unrelated_commands_are_not_refreshes(self) -> None:
        for args in (
            ["pr", "view", "7"],
            ["pr", "update-branch", "--help"],
            ["api", "repos/o/r/pulls/7"],
            ["api", "graphql", "-f", "query=query{viewer{login}}"],
        ):
            with self.subTest(args=args):
                self.assertIsNone(guard.refresh_request(args))


class EndToEnd(unittest.TestCase):
    """``main`` with the real-gh boundary replaced by canned responses."""

    def run_main(
        self,
        args: list[str],
        *,
        config: str | None,
        state: dict[str, Any] | None = None,
        env: dict[str, str] | None = None,
    ) -> tuple[int, str, list[list[str]]]:
        calls: list[list[str]] = []

        def fake(arguments: list[str]) -> tuple[int, str, str]:
            calls.append(arguments)
            joined = " ".join(arguments)
            if "contents/.shipyard/config.toml" in joined:
                if config is None:
                    return 1, "", 'gh: Not Found (HTTP 404)\n'
                return 0, config, ""
            if arguments[:2] == ["api", "graphql"]:
                return 0, json.dumps(state if state is not None else pr_response()), ""
            if arguments[:1] == ["api"] and arguments[-1].startswith("repos/"):
                return 0, json.dumps({"base": {"ref": "main"}}), ""
            return 1, "", "unexpected call"

        stderr = io.StringIO()
        with (
            mock.patch.object(guard, "_run_real_gh_text", side_effect=fake),
            mock.patch.object(guard, "_BASE_CACHE", {}),
            mock.patch.object(guard.time, "sleep"),
            mock.patch.dict(os.environ, env or {}, clear=False),
            contextlib.redirect_stderr(stderr),
        ):
            code = guard.main(args)
        return code, stderr.getvalue(), calls

    ENABLED = "[merge]\nrefresh_branch = \"only-if-conflicting\"\n"
    ARGS = ["pr", "update-branch", "8885", "--repo", "Generous-Corp/pulp"]

    def test_enabled_policy_refuses_a_pointless_refresh(self) -> None:
        code, stderr, calls = self.run_main(self.ARGS, config=self.ENABLED)
        self.assertEqual(code, 1)
        self.assertIn("refusing", stderr)
        self.assertTrue(
            any("contents/.shipyard/config.toml?ref=main" in " ".join(call) for call in calls),
            "the policy must be read from the PR's base branch",
        )

    def test_absent_config_allows_without_reading_pr_state(self) -> None:
        code, stderr, calls = self.run_main(self.ARGS, config=None)
        self.assertEqual(code, 0, stderr)
        self.assertFalse(any(call[:2] == ["api", "graphql"] for call in calls))

    def test_default_policy_allows(self) -> None:
        code, _, _ = self.run_main(self.ARGS, config="[merge]\nrefresh_branch = \"always\"\n")
        self.assertEqual(code, 0)

    def test_enabled_policy_still_allows_a_conflict_fix(self) -> None:
        state = pr_response(mergeable="CONFLICTING", merge_state="DIRTY")
        code, stderr, _ = self.run_main(self.ARGS, config=self.ENABLED, state=state)
        self.assertEqual(code, 0, stderr)

    def test_unknown_mergeability_is_reread_then_allowed(self) -> None:
        state = pr_response(mergeable="UNKNOWN", merge_state="UNKNOWN")
        code, _, calls = self.run_main(self.ARGS, config=self.ENABLED, state=state)
        self.assertEqual(code, 0)
        graphql_reads = [call for call in calls if call[:2] == ["api", "graphql"]]
        self.assertEqual(len(graphql_reads), 1 + guard.UNKNOWN_MERGEABLE_RETRIES)

    def test_operator_override_allows_with_a_warning(self) -> None:
        code, stderr, _ = self.run_main(
            self.ARGS, config=self.ENABLED, env={"GHAPP_ALLOW_BRANCH_REFRESH": "1"}
        )
        self.assertEqual(code, 0)
        self.assertIn("WARNING", stderr)

    def test_unrelated_command_makes_no_github_call(self) -> None:
        code, _, calls = self.run_main(["pr", "view", "8885"], config=self.ENABLED)
        self.assertEqual(code, 0)
        self.assertEqual(calls, [])


class InstalledLayout(unittest.TestCase):
    def test_guard_finds_its_parser_next_to_it_when_installed(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            guards = pathlib.Path(temp)
            removal = SCRIPT.with_name("ghapp_queue_removal_guard.py")
            (guards / "queue-removal-guard").write_bytes(removal.read_bytes())
            (guards / "branch-refresh-guard").write_bytes(SCRIPT.read_bytes())
            spec = importlib.util.spec_from_loader(
                "installed_refresh_guard",
                importlib.machinery.SourceFileLoader(
                    "installed_refresh_guard", str(guards / "branch-refresh-guard")
                ),
            )
            assert spec is not None and spec.loader is not None
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
            self.assertIsNotNone(module.PARSER)
            self.assertEqual(
                module.refresh_request(["api", "-X", "PUT", "repos/o/r/pulls/3/update-branch"]),
                [("o", "r", 3)],
            )


if __name__ == "__main__":
    unittest.main()

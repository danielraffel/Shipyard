"""Guard mode: a head is authorized by the fleet admission check run, not comments."""

from __future__ import annotations

import importlib.util
import json
import pathlib
import tempfile
import unittest
from typing import Any
from unittest import mock

HERE = pathlib.Path(__file__).resolve().parent


def _load(name: str) -> Any:
    spec = importlib.util.spec_from_file_location(name, HERE / f"{name}.py")
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


guard = _load("ghapp_queue_arm_guard")
status = _load("review_verdict_status")

HEAD = "a" * 40
OTHER = "b" * 40
POLICY_SHA = "c" * 40
GUARD_APP = 4242
CHECK = "fleet-guard/admission"


def run(run_id: int = 10, *, app: int = GUARD_APP, slug: str = "fleet-guard",
        head: str = HEAD, status_: str = "completed", conclusion: str | None = "success",
        external: Any = None, name: str = CHECK) -> dict[str, Any]:
    if external is None:
        external = json.dumps({"base_sha": OTHER, "head_sha": head, "policy_sha": POLICY_SHA,
                               "classification": "routine"})
    return {"id": run_id, "name": name, "head_sha": head, "status": status_,
            "conclusion": conclusion, "external_id": external,
            "app": {"id": app, "slug": slug}}


def judge(runs: list[dict[str, Any]]) -> tuple[str, str, Any, list[str]]:
    notes: list[str] = []
    state, why, approval = guard.judge_admission(runs, HEAD, CHECK, GUARD_APP, notes)
    return state, why, approval, notes


class ApprovalSource(unittest.TestCase):
    def test_comments_is_the_default_and_the_explicit_fallback(self) -> None:
        self.assertEqual(guard.approval_source({}), {"source": "comments"})
        self.assertEqual(guard.approval_source({"approval_source": "comments"}),
                         {"source": "comments"})

    def test_guard_needs_a_positive_non_actions_app_id(self) -> None:
        self.assertEqual(
            guard.approval_source({"approval_source": "guard", "guard_app_id": GUARD_APP}),
            {"source": "guard", "check": CHECK, "app_id": GUARD_APP})
        for bad in ({"approval_source": "guard"},
                    {"approval_source": "guard", "guard_app_id": 0},
                    {"approval_source": "guard", "guard_app_id": "4242"},
                    {"approval_source": "guard", "guard_app_id": True},
                    {"approval_source": "guard", "guard_app_id": 15368},
                    {"approval_source": "guard", "guard_app_id": GUARD_APP,
                     "guard_admission_check": " "},
                    {"approval_source": "either"}):
            with self.subTest(bad=bad):
                self.assertEqual(guard.approval_source(bad)["source"], "misconfigured")

    def test_guard_mode_requires_authorization_even_when_arming_is_not_required(self) -> None:
        text = ('[auto_merge]\narm_requires_head_approval = false\n'
                f'approval_source = "guard"\nguard_app_id = {GUARD_APP}\n')
        policy = guard.review_policy(text, "owner/repo", False)
        self.assertTrue(policy["required"])
        self.assertEqual(policy["source"]["source"], "guard")


class Judge(unittest.TestCase):
    def test_a_bound_successful_run_from_the_guard_app_admits(self) -> None:
        state, _, approval, _ = judge([run()])
        self.assertEqual(state, "admitted")
        self.assertEqual(approval["admission_run_id"], 10)
        self.assertEqual(approval["policy_sha"], POLICY_SHA)

    def test_runs_from_any_other_app_never_count(self) -> None:
        for forged in (run(app=15368, slug="github-actions"),
                       run(app=3878000, slug="shipyard-local"),
                       run(app=GUARD_APP, slug="shipyard-local")):
            with self.subTest(app=forged["app"]):
                state, _, _, notes = judge([forged])
                self.assertEqual(state, "waiting")
                self.assertTrue(notes and notes[0].startswith("excluded_admission:"), notes)

    def test_the_newest_counting_run_decides(self) -> None:
        self.assertEqual(judge([run(10), run(11, conclusion="failure")])[0], "refused")
        self.assertEqual(judge([run(11, conclusion="failure"), run(12)])[0], "admitted")
        self.assertEqual(judge([run(12), run(13, status_="in_progress", conclusion=None)])[0],
                         "waiting")

    def test_an_unbound_record_counts_as_absent(self) -> None:
        for external in ("", "not json", json.dumps({"head_sha": OTHER, "policy_sha": POLICY_SHA,
                                                     "classification": "routine"}),
                         json.dumps({"head_sha": HEAD, "policy_sha": "abc",
                                     "classification": "routine"}),
                         json.dumps({"head_sha": HEAD, "policy_sha": POLICY_SHA})):
            with self.subTest(external=external):
                self.assertEqual(judge([run(external=external)])[0], "waiting")

    def test_a_run_on_another_head_or_of_another_name_is_ignored(self) -> None:
        self.assertEqual(judge([run(head=OTHER)])[0], "waiting")
        self.assertEqual(judge([run(name="review-verdict")])[0], "waiting")
        self.assertEqual(judge([])[0], "waiting")


class Gate(unittest.TestCase):
    def gate(self, source: dict[str, Any], runs: list[dict[str, Any]] | None = None,
             require_file: bool = False) -> tuple[bool, str, Any, dict[str, Any]]:
        target: dict[str, Any] = {}
        with tempfile.TemporaryDirectory() as guards, \
                mock.patch.object(guard, "_guards_dir", return_value=pathlib.Path(guards)), \
                mock.patch.object(guard, "_rest_check_runs", return_value=runs or []):
            if require_file:
                (pathlib.Path(guards) / guard.HOST_REQUIRE_GUARD_FILE).write_text("1\n")
            result = guard.admission_gate("owner/repo", 7, HEAD, source, target)
        if result is None:
            return None, "", None, target  # type: ignore[return-value]
        allowed, message, fix = result
        return allowed, message, fix, target

    def test_comment_mode_is_left_to_the_comment_rule(self) -> None:
        self.assertIsNone(self.gate({"source": "comments"})[0])

    def test_a_host_that_requires_guard_refuses_comment_mode(self) -> None:
        allowed, _, fix, _ = self.gate({"source": "comments"}, require_file=True)
        self.assertFalse(allowed)
        self.assertEqual(fix["cause"], "admission_missing")

    def test_guard_mode_admits_only_on_the_guard_run(self) -> None:
        source = {"source": "guard", "check": CHECK, "app_id": GUARD_APP}
        allowed, _, _, target = self.gate(source, [run()])
        self.assertTrue(allowed)
        self.assertEqual(target["approval"]["admission_app_id"], GUARD_APP)
        allowed, _, fix, _ = self.gate(source, [run(app=3878000, slug="shipyard-local")])
        self.assertFalse(allowed)
        self.assertEqual(fix["cause"], "admission_missing")
        allowed, _, fix, _ = self.gate(source, [run(conclusion="failure")])
        self.assertFalse(allowed)
        self.assertEqual(fix["cause"], "admission_refused")

    def test_a_misconfigured_guard_policy_refuses(self) -> None:
        allowed, _, _, _ = self.gate({"source": "misconfigured", "detail": "no app id"})
        self.assertFalse(allowed)

    def test_an_unreadable_admission_check_refuses(self) -> None:
        source = {"source": "guard", "check": CHECK, "app_id": GUARD_APP}
        with mock.patch.object(guard, "_rest_check_runs",
                               side_effect=guard.GuardError("HTTP 502")):
            allowed, _, fix = guard.admission_gate("owner/repo", 7, HEAD, source, {})
        self.assertFalse(allowed)
        self.assertEqual(fix["cause"], "unreadable")


class Status(unittest.TestCase):
    def test_the_admission_check_run_event_names_its_head(self) -> None:
        event = {"check_run": {"name": CHECK, "head_sha": HEAD,
                               "pull_requests": [{"number": 7}]}}
        self.assertEqual(status.event_target("check_run", event), (7, HEAD))
        self.assertEqual(status.event_target("check_run", {"check_run": {"head_sha": HEAD}}),
                         (None, HEAD))

    def test_waiting_for_admission_is_pending_not_failure(self) -> None:
        decision = status.Decision(False, "PR #7: not admitted", "admission_missing", ())
        self.assertTrue(decision.awaiting)
        self.assertIn("fleet-guard admission",
                      status.awaiting_description(HEAD, [], admission=True))
        refused = status.Decision(False, "PR #7: not admitted", "admission_refused", ())
        self.assertFalse(refused.awaiting)


if __name__ == "__main__":
    unittest.main()

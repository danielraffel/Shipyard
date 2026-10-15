"""The ``review-verdict`` status: the guard's decision, posted on the live head.

Each case runs the script as the workflow does (a real subprocess, a fake
``gh`` answering from the review-before-arm captures) and, for parity, runs
the installed ``queue-arm-guard`` on the same answers: the status is
``success`` exactly when the guard lets the head land.
"""

from __future__ import annotations

import copy
import json
import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest
from typing import Any

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import test_ghapp_queue_arm_guard as arm  # noqa: E402

STATUS = pathlib.Path(__file__).with_name("review_verdict_status.py")
REPO = "danielraffel/Shipyard"
HEAD = arm.HEAD_784


class RepositorySideSwitches(unittest.TestCase):
    """The status pins the guard's switches to the repository-side rule."""

    def test_the_rebase_carry_and_review_are_on_whatever_the_host_says(self) -> None:
        import importlib.util
        import os
        from unittest import mock
        spec = importlib.util.spec_from_file_location("review_verdict_status", STATUS)
        assert spec is not None and spec.loader is not None
        status = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(status)
        seen: dict[str, str | None] = {}

        class Stop(Exception):
            pass

        def load_guard() -> None:
            seen.update({name: os.environ.get(name) for name in
                         ("SHIPYARD_REBASE_CARRY", "SHIPYARD_REVIEW_BEFORE_ARM")})
            raise Stop

        event = {"pull_request": {"number": 784, "head": {"sha": "a" * 40}}}
        with mock.patch.dict(os.environ, {"SHIPYARD_REBASE_CARRY": "0",
                                          "SHIPYARD_REVIEW_BEFORE_ARM": "0"}), \
                mock.patch.object(status, "load_guard", load_guard), \
                self.assertRaises(Stop):
            status.run("pull_request_target", event, "danielraffel/Shipyard", None)
        self.assertEqual(seen, {"SHIPYARD_REBASE_CARRY": "1", "SHIPYARD_REVIEW_BEFORE_ARM": "1"})


class ReviewVerdictStatus(unittest.TestCase):
    install = arm.ReviewBeforeArm.install
    case = arm.ReviewBeforeArm.case
    run_guard = arm.ReviewBeforeArm.run_guard
    calls = arm.ReviewBeforeArm.calls

    def setUp(self) -> None:
        self.root = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root, True)
        self.guards = self.root / "guards"
        self.guards.mkdir()
        shutil.copy(arm.SCRIPT, self.guards / "queue-arm-guard")
        shutil.copy(arm.REMOVAL, self.guards / "queue-removal-guard")
        self.scripts = self.root / "scripts"
        self.scripts.mkdir()
        shutil.copy(STATUS, self.scripts / STATUS.name)
        shutil.copy(arm.SCRIPT, self.scripts / arm.SCRIPT.name)
        shutil.copy(arm.REMOVAL, self.scripts / arm.REMOVAL.name)

    def answers(self, trail: list[dict[str, Any]], config: str | None = None
                ) -> list[tuple[str, dict[str, Any]]]:
        pull = copy.deepcopy(arm.review_fixture("shipyard-784-pull.json"))
        pull["state"] = "open"
        answers = self.case("shipyard-784", queue=False, comments=[trail], config=config,
                            graphql=arm.opened(arm.review_fixture("shipyard-784-graphql.json")))
        answers.insert(0, ("/statuses/", {"json": {"state": "posted"}}))
        answers.insert(0, (f"/commits/{HEAD}/pulls", {"json": [
            {"number": 784, "state": "open", "head": {"sha": HEAD}},
            {"number": 700, "state": "closed", "head": {"sha": HEAD}},
            {"number": 701, "state": "open", "head": {"sha": arm.HOLD_784}}]}))
        return [(needle, {"json": pull}) if needle == "/pulls/" else (needle, answer)
                for needle, answer in answers]

    def comment(self, body: str, at: str | None, **change: Any) -> dict[str, Any]:
        template = next(c for c in arm.review_fixture("shipyard-784-comments.json")[0]
                        if c["id"] == arm.APPROVE_784_COMMENT)
        return arm.comment_like(template, body=body, created_at=at, **change)

    def post(self, answers: list[tuple[str, dict[str, Any]]], event_name: str = "issue_comment",
             event: dict[str, Any] | None = None) -> subprocess.CompletedProcess:
        (self.root / "calls").unlink(missing_ok=True)
        fake = self.install(answers)
        payload = event if event is not None else {
            "issue": {"number": 784, "pull_request": {"url": "x"}}}
        event_path = self.root / "event.json"
        event_path.write_text(json.dumps(payload), encoding="utf-8")
        environment = {"PATH": "/usr/bin:/bin", "GHAPP_REAL_GH": str(fake), "HOME": str(self.root),
                       "GITHUB_EVENT_NAME": event_name, "GITHUB_EVENT_PATH": str(event_path),
                       "GITHUB_REPOSITORY": REPO, "GITHUB_RUN_ID": "99",
                       # A host's own switches must not reach the status.
                       "SHIPYARD_REBASE_CARRY": "0", "SHIPYARD_REVIEW_BEFORE_ARM": "0"}
        return subprocess.run([sys.executable, str(self.scripts / STATUS.name)], env=environment,
                              capture_output=True, text=True, check=False,
                              stdin=subprocess.DEVNULL)

    def posted(self) -> dict[str, str]:
        lines = [line for line in self.calls().splitlines() if "/statuses/" in line]
        self.assertEqual(len(lines), 1, self.calls())
        fields = dict(part.split("=", 1) for part in lines[0].split(" -f ")[1:])
        fields["url"] = lines[0].split()[3]
        return fields

    def judged(self, trail: list[dict[str, Any]], config: str | None = None) -> str:
        """Post the status, run the guard on the same answers, and return the
        status after checking they agree."""
        result = self.post(self.answers(trail, config))
        self.assertEqual(result.returncode, 0, result.stderr)
        posted = self.posted()
        self.assertEqual(posted["url"], f"repos/{REPO}/statuses/{HEAD}")
        self.assertEqual(posted["context"], "review-verdict")
        self.assertLessEqual(len(posted["description"]), 140)
        self.assertEqual(posted["target_url"], f"https://github.com/{REPO}/actions/runs/99")
        guard = self.run_guard(["pr", "merge", "784", "--merge", "--match-head-commit", HEAD],
                               self.answers(trail, config), REPO)
        self.assertEqual(posted["state"] == "success", guard.returncode == 0,
                         f"status {posted['state']} disagrees with the guard: {guard.stderr}")
        return posted["state"]

    def test_a_base_signer_list_makes_the_status_require_a_signer(self) -> None:
        # The status reads no host's signers file; the base config's list
        # applies the same rule server-side.
        config = '[auto_merge]\nreviewer_signers = ["pulp-review", "infra-review2"]\n'
        unsigned = [self.comment(f"verdict: APPROVE head:{HEAD}", "2026-10-12T15:00:00Z")]
        self.assertEqual(self.judged(unsigned, config), "failure")
        signed = [self.comment(f"Reviewed.\nreviewer: infra-review2\nverdict: APPROVE head:{HEAD}",
                               "2026-10-12T15:00:00Z")]
        self.assertEqual(self.judged(signed, config), "success")

    def test_an_approve_naming_the_head_is_success(self) -> None:
        trail = [self.comment(f"verdict: APPROVE head:{HEAD}", "2026-10-12T15:00:00Z")]
        self.assertEqual(self.judged(trail), "success")
        self.assertTrue(self.posted()["description"].startswith("approved: "))

    def test_a_hold_after_an_approve_is_failure(self) -> None:
        trail = [self.comment(f"verdict: APPROVE head:{HEAD}", "2026-10-12T15:00:00Z"),
                 self.comment(f"verdict: HOLD head:{HEAD}", "2026-10-12T16:00:00Z", id=2)]
        self.assertEqual(self.judged(trail), "failure")
        self.assertIn("HOLD", self.posted()["description"])

    def test_an_untimed_hold_beats_a_newer_approve(self) -> None:
        trail = [self.comment(f"verdict: HOLD head:{HEAD}", None),
                 self.comment(f"verdict: APPROVE head:{HEAD}", "2026-10-12T17:00:00Z", id=2)]
        self.assertEqual(self.judged(trail), "failure")

    def test_an_approve_from_an_untrusted_account_is_failure(self) -> None:
        # A fork's author or a drive-by account cannot forge the status.
        trail = [self.comment(f"verdict: APPROVE head:{HEAD}", "2026-10-12T15:00:00Z",
                              user={"login": "drive-by", "type": "User"},
                              author_association="NONE")]
        self.assertEqual(self.judged(trail), "failure")

    def test_an_approve_of_another_head_is_failure(self) -> None:
        trail = [self.comment(f"verdict: APPROVE head:{arm.HOLD_784}", "2026-10-12T15:00:00Z")]
        self.assertEqual(self.judged(trail), "failure")

    def test_a_repository_that_does_not_require_review_is_success(self) -> None:
        config = "[auto_merge]\narm_requires_head_approval = false\n"
        self.assertEqual(self.judged([], config), "success")

    def test_a_comment_on_an_issue_posts_nothing(self) -> None:
        result = self.post([], event={"issue": {"number": 5}})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / "calls").exists())

    def test_a_review_judges_the_live_head_through_workflow_run(self) -> None:
        trail = [self.comment(f"verdict: APPROVE head:{HEAD}", "2026-10-12T15:00:00Z")]
        event = {"workflow_run": {"pull_requests": [{"number": 784}], "head_sha": arm.HOLD_784}}
        result = self.post(self.answers(trail), "workflow_run", event)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.posted()["url"], f"repos/{REPO}/statuses/{HEAD}")
        self.assertEqual(self.posted()["state"], "success")

    def test_a_fork_review_with_no_listed_pull_request_judges_its_head(self) -> None:
        trail = [self.comment(f"verdict: HOLD head:{HEAD}", "2026-10-12T15:00:00Z")]
        event = {"workflow_run": {"pull_requests": [], "head_sha": HEAD}}
        result = self.post(self.answers(trail), "workflow_run", event)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"/commits/{HEAD}/pulls", self.calls())
        self.assertEqual(self.posted()["url"], f"repos/{REPO}/statuses/{HEAD}")
        self.assertEqual(self.posted()["state"], "failure")

    def test_a_review_event_is_never_run_directly(self) -> None:
        # pull_request_review runs the pull request's own workflow file, so the
        # poster must not accept it.
        result = self.post([], "pull_request_review", {"pull_request": {"number": 784}})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsupported event", result.stderr)
        workflow = (STATUS.parent.parent / ".github" / "workflows" / "review-verdict.yml").read_text()
        on = workflow.split("\non:", 1)[1].split("\npermissions:", 1)[0]
        self.assertNotIn("pull_request_review", on)
        trigger = (STATUS.parent.parent / ".github" / "workflows"
                   / "review-verdict-trigger.yml").read_text()
        self.assertIn("permissions: {}", trigger)

    def test_every_trigger_queues_in_one_group_per_pull_request(self) -> None:
        # A review's run and a comment's run on one pull request must share a
        # concurrency group, or a stale success can land after a newer HOLD.
        root = STATUS.parent.parent
        for path in (root / ".github" / "workflows" / "review-verdict.yml",
                     root / "docs" / "review-verdict-status.md"):
            text = path.read_text(encoding="utf-8")
            groups = [line.strip() for line in text.splitlines()
                      if line.strip().startswith("group: review-verdict-")]
            self.assertEqual(groups, [
                "group: review-verdict-${{ github.event.pull_request.number || "
                "github.event.issue.number || github.event.workflow_run.pull_requests[0].number "
                "|| github.event.workflow_run.head_sha }}"], path)

    def test_a_head_two_open_pull_requests_share_needs_both_approved(self) -> None:
        trail = [self.comment(f"verdict: APPROVE head:{HEAD}", "2026-10-12T15:00:00Z")]
        answers = self.answers(trail)
        pull = copy.deepcopy(arm.review_fixture("shipyard-784-pull.json"))
        pull.update(state="open", number=785)
        answers[:0] = [
            (f"/commits/{HEAD}/pulls", {"json": [
                {"number": 784, "state": "open", "head": {"sha": HEAD}},
                {"number": 785, "state": "open", "head": {"sha": HEAD}}]}),
            ("issues/785/comments", {"pages": [[]]}),
            ("pulls/785/reviews", {"pages": [[]]}),
            ("pulls/785", {"json": pull}),
        ]
        result = self.post(answers)
        self.assertEqual(result.returncode, 0, result.stderr)
        posted = self.posted()
        self.assertEqual(posted["state"], "failure", result.stdout)
        self.assertIn("PR #784: allowed", result.stdout)
        self.assertIn("PR #785: refused", result.stdout)
        # Control: with only #784 on the head, the same trail is success.
        self.assertEqual(self.judged(trail), "success")

    def test_a_closed_pull_request_posts_nothing(self) -> None:
        answers = self.answers([])
        closed = [(needle, {"json": dict(answer["json"], state="closed")})
                  if needle == "/pulls/" else (needle, answer) for needle, answer in answers]
        result = self.post(closed)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("/statuses/", self.calls())


if __name__ == "__main__":
    unittest.main()

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
SIGNERS = '[auto_merge]\nreviewer_signers = ["pulp-review", "infra-review2"]\n'
# The stamp ghapp adds on the reviewer's host; the status refuses an App
# APPROVE without one.
STAMP = "<!-- session: Daniels-Mac-Studio-m3/review-1 -->"
SIGNED_APPROVE = f"Reviewed.\nreviewer: pulp-review\n{STAMP}\nverdict: APPROVE head:{{head}}"
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


class AwaitingClassification(unittest.TestCase):
    """Which refusals read as waiting, and how an uncounted approval is named."""

    @classmethod
    def setUpClass(cls) -> None:
        import importlib.util
        spec = importlib.util.spec_from_file_location("review_verdict_status", STATUS)
        assert spec is not None and spec.loader is not None
        cls.status = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.status)

    def test_only_a_missing_counted_verdict_is_awaiting(self) -> None:
        decision = self.status.Decision
        self.assertTrue(decision(False, "no verdict", "unreviewed", ()).awaiting)
        self.assertTrue(decision(False, "stale", "approval_does_not_carry", ()).awaiting)
        for cause in ("hold", "unreadable", "head_moved", "unbound", "self_approval", None):
            self.assertFalse(decision(False, "x", cause, ()).awaiting, cause)
        self.assertFalse(decision(True, "ok", None, ()).awaiting)
        # A receipt the guard could not prove shares the "unreviewed" cause but
        # is a refusal of an approval that exists, not a wait.
        self.assertFalse(decision(False, "PR #1: reviewer claim receipt refused (x)",
                                  "unreviewed", ()).awaiting)

    def test_exclusion_codes_name_why_an_approval_did_not_count(self) -> None:
        code = self.status.exclusion_code
        self.assertEqual(code("its session stamp is unattributed"), "unattributed")
        self.assertEqual(code("it has no `ghapp` session stamp"), "unattributed")
        self.assertEqual(code("its stamp session s is not one of the base's "
                              "`reviewer_stamp_sessions` (a-*)"), "unlisted_stamp_session")
        self.assertEqual(code("its signer x is not one of a, b"), "uncounted_signer")
        self.assertEqual(code("not the App: a user account"), "not_the_app")
        self.assertEqual(code("drive-by"), "unlisted_human")

    def test_the_description_stays_within_githubs_limit(self) -> None:
        text = self.status.awaiting_description("a" * 40, ["unattributed"] * 30 + ["x" * 200])
        self.assertLessEqual(len(text), self.status.DESCRIPTION_LIMIT)
        self.assertTrue(text.startswith("awaiting reviewer verdict; 31 approvals excluded"))


def plain_comment_event() -> dict[str, Any]:
    """An ``issue_comment`` payload on PR 784 whose comment is no verdict."""
    return {"action": "created",
            "comment": {"id": 1, "body": "thanks", "user": {"login": "someone"}},
            "issue": {"number": 784, "pull_request": {"url": "x"}}}


class ReviewVerdictStatus(unittest.TestCase):
    install = arm.ReviewBeforeArm.install
    case = arm.ReviewBeforeArm.case
    run_guard = arm.ReviewBeforeArm.run_guard
    calls = arm.ReviewBeforeArm.calls

    def setUp(self) -> None:
        self.root = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root, True)
        # Statuses already on the head (``held_in_history``): none by default.
        self.history: list[dict[str, Any]] = []
        self.guards = self.root / "guards"
        self.guards.mkdir()
        shutil.copy(arm.SCRIPT, self.guards / "queue-arm-guard")
        shutil.copy(arm.REMOVAL, self.guards / "queue-removal-guard")
        self.scripts = self.root / "scripts"
        self.scripts.mkdir()
        shutil.copy(STATUS, self.scripts / STATUS.name)
        shutil.copy(arm.SCRIPT, self.scripts / arm.SCRIPT.name)
        shutil.copy(arm.REMOVAL, self.scripts / arm.REMOVAL.name)

    def permissive_guards(self) -> pathlib.Path:
        directory = self.root / "host-guards"
        directory.mkdir(exist_ok=True)
        (directory / "reviewer-signers.json").write_text(
            json.dumps({"default": ["codex-g2h", "pulp-review", "infra-review2"]}),
            encoding="utf-8")
        return directory

    def answers(self, trail: list[dict[str, Any]], config: str | None = None
                ) -> list[tuple[str, dict[str, Any]]]:
        pull = copy.deepcopy(arm.review_fixture("shipyard-784-pull.json"))
        pull["state"] = "open"
        answers = self.case("shipyard-784", queue=False, comments=[trail], config=config,
                            graphql=arm.opened(arm.review_fixture("shipyard-784-graphql.json")))
        answers.insert(0, ("/statuses/", {"json": {"state": "posted"}}))
        # The head's status history, read for a HOLD this job posted before.
        answers.insert(0, (f"/commits/{HEAD}/statuses?", {"pages": [self.history]}))
        # The head's own pull_request_target run, which a comment or review
        # re-runs so its check follows the verdict.
        answers.insert(0, ("/actions/runs/555/rerun", {"text": ""}))
        answers.insert(0, ("actions/runs?head_sha", {"pages": [{"total_count": 3, "workflow_runs": [
            {"id": 500, "name": "Review verdict", "event": "pull_request_target"},
            {"id": 555, "name": "Review verdict", "event": "pull_request_target"},
            {"id": 600, "name": "Other", "event": "pull_request_target"}]}]}))
        answers.insert(0, (f"/commits/{HEAD}/pulls", {"pages": [[
            {"number": 784, "state": "open", "head": {"sha": HEAD}},
            {"number": 700, "state": "closed", "head": {"sha": HEAD}},
            {"number": 701, "state": "open", "head": {"sha": arm.HOLD_784}}]]}))
        return [(needle, {"json": pull}) if needle == "/pulls/" else (needle, answer)
                for needle, answer in answers]

    def comment(self, body: str, at: str | None, **change: Any) -> dict[str, Any]:
        template = next(c for c in arm.review_fixture("shipyard-784-comments.json")[0]
                        if c["id"] == arm.APPROVE_784_COMMENT)
        return arm.comment_like(template, body=body, created_at=at, **change)

    def post(self, answers: list[tuple[str, dict[str, Any]]], event_name: str = "issue_comment",
             event: dict[str, Any] | None = None, ref: str = "refs/heads/main",
             **env: str) -> subprocess.CompletedProcess:
        (self.root / "calls").unlink(missing_ok=True)
        fake = self.install(answers)
        payload = event if event is not None else plain_comment_event()
        event_path = self.root / "event.json"
        payload.setdefault("repository", {"default_branch": "main"})
        event_path.write_text(json.dumps(payload), encoding="utf-8")
        environment = {"PATH": "/usr/bin:/bin", "GHAPP_REAL_GH": str(fake), "HOME": str(self.root),
                       "GITHUB_EVENT_NAME": event_name, "GITHUB_EVENT_PATH": str(event_path),
                       "GITHUB_REPOSITORY": REPO, "GITHUB_RUN_ID": "99", "GITHUB_REF": ref,
                       "TMPDIR": str(self.root),
                       # A host's own switches must not reach the status.
                       "SHIPYARD_REBASE_CARRY": "0", "SHIPYARD_REVIEW_BEFORE_ARM": "0",
                       # Nor its signers file, which would allow anyone here.
                       "SHIPYARD_GHAPP_GUARDS_DIR": str(self.permissive_guards())}
        environment.update(env)
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
                               self.answers(trail, config), REPO,
                               SHIPYARD_SIGNERS_FROM_BASE="1")
        self.assertEqual(posted["state"] == "success", guard.returncode == 0,
                         f"status {posted['state']} disagrees with the guard: {guard.stderr}")
        return posted["state"]

    def test_signers_come_only_from_the_base_whatever_the_host_lists(self) -> None:
        # Every case runs beside a host signers file that allows codex-g2h,
        # pulp-review and infra-review2; the status must not read it.
        def approve(signer: str | None) -> list[dict[str, Any]]:
            line = f"reviewer: {signer}\n" if signer else ""
            return [self.comment(f"Reviewed.\n{line}{STAMP}\nverdict: APPROVE head:{HEAD}",
                                 "2026-10-12T15:00:00Z")]
        self.assertEqual(self.judged(approve(None), SIGNERS), "pending", "unsigned")
        self.assertEqual(self.judged(approve("codex-g2h"), SIGNERS), "pending", "unlisted")
        self.assertEqual(self.judged(approve("pulp-review"), SIGNERS), "success", "listed")
        self.assertEqual(self.judged(approve("pulp-review"), None), "pending", "none listed")
        result = self.post(self.answers(approve("pulp-review"), None))
        self.assertIn("no signer list is configured on the base branch", result.stderr)

    def test_a_base_signer_list_makes_the_status_require_a_signer(self) -> None:
        # The status reads no host's signers file; the base config's list
        # applies the same rule server-side.
        config = '[auto_merge]\nreviewer_signers = ["pulp-review", "infra-review2"]\n'
        unsigned = [self.comment(f"verdict: APPROVE head:{HEAD}", "2026-10-12T15:00:00Z")]
        self.assertEqual(self.judged(unsigned, config), "pending")
        signed = [self.comment(f"Reviewed.\nreviewer: infra-review2\n{STAMP}\n"
                               f"verdict: APPROVE head:{HEAD}", "2026-10-12T15:00:00Z")]
        self.assertEqual(self.judged(signed, config), "success")

    def test_base_signer_identity_binding_is_enforced_by_the_status(self) -> None:
        config = ('[auto_merge]\n'
                  'reviewer_signers = ["codex-review"]\n'
                  '[auto_merge.reviewer_signer_identities]\n'
                  'codex-review = ["Daniels-Mac-Studio-m3 agent:codex-reviewer"]\n')

        def approve(stamp: str) -> list[dict[str, Any]]:
            return [self.comment(f"Reviewed.\nreviewer: codex-review\n{stamp}\n"
                                 f"verdict: APPROVE head:{HEAD}", "2026-10-12T15:00:00Z")]

        good = "<!-- session: Daniels-Mac-Studio-m3/rotating agent: codex-reviewer -->"
        wrong_host = "<!-- session: Daniels-M5-Studio/rotating agent: codex-reviewer -->"
        no_agent = "<!-- session: Daniels-Mac-Studio-m3/rotating -->"
        self.assertEqual(self.judged(approve(good), config), "success")
        self.assertEqual(self.judged(approve(wrong_host), config), "pending")
        self.assertEqual(self.judged(approve(no_agent), config), "pending")

    def test_pulp_11407s_author_lane_approve_fails_where_the_base_lists_reviewer_sessions(
            self) -> None:
        # pulp#11407: the authoring session's APPROVE (comment 6568210591),
        # signed `reviewer: pulp-review` under its own session stamp, turned
        # the status green. With the base listing the reviewer runs' session
        # globs only such a run's APPROVE does; a HOLD from any session fails it.
        config = ('[auto_merge]\n'
                  'reviewer_signers = ["pulp-review", "infra-review2"]\n'
                  'reviewer_stamp_sessions = ["fleet-pass-reviewer-*"]\n')
        author = ("<!-- session: Daniels-Mac-Studio-m3/01a18030-f638-7010-94b9-2198e940232f "
                  "agent: pulp-review -->")
        reviewer = ("<!-- session: Daniels-Mac-Studio-m3/fleet-pass-reviewer-Daniels-Mac-Studio-m3-"
                    "20261102T033922Z-92868 agent: pulp-review -->")

        def verdict(stamp: str, verdict: str, at: str, **change: Any) -> dict[str, Any]:
            return self.comment(f"Reviewed exact head {HEAD}.\n\nreviewer: pulp-review\n{stamp}\n"
                                f"verdict: {verdict} head:{HEAD}", at, **change)

        self.assertEqual(self.judged([verdict(author, "APPROVE", "2026-11-02T03:18:19Z")], config),
                         "pending")
        self.assertEqual(self.posted()["description"],
                         "awaiting reviewer verdict; 1 approval excluded: unlisted_stamp_session")
        result = self.post(self.answers([verdict(author, "APPROVE", "2026-11-02T03:18:19Z")],
                                        config))
        self.assertIn("its stamp session 01a18030-f638-7010-94b9-2198e940232f is not one of the "
                      "base's `reviewer_stamp_sessions` (fleet-pass-reviewer-*)", result.stderr)
        self.assertEqual(self.judged([verdict(reviewer, "APPROVE", "2026-11-02T03:55:50Z")],
                                     config), "success", "control: the reviewer run counts")
        self.assertEqual(self.judged([verdict(reviewer, "APPROVE", "2026-11-02T03:55:50Z"),
                                      verdict(author, "HOLD", "2026-11-02T04:00:00Z", id=2)],
                                     config), "failure", "the author lane's HOLD still holds")
        self.assertIn("HOLD", self.posted()["description"])
        self.assertEqual(self.judged([verdict(author, "APPROVE", "2026-11-02T03:18:19Z")],
                                     SIGNERS), "success",
                         "with no list on the base the rule is off: what pulp#11407 saw")

    def test_tartci_623s_copied_approve_fails_stamped_unattributed_or_unstamped(self) -> None:
        # tartci#623's deciding APPROVE (comment 6321450475) copied
        # infra-review2's signer and session lines under a
        # Daniels-M5-Studio/unattributed stamp. Posted around ghapp's stamper
        # it would carry no stamp; neither form may pass the status.
        def copied(stamp: str) -> list[dict[str, Any]]:
            return [self.comment(
                "Exact-head re-review completed.\n\nreviewer: infra-review2\n"
                "session: infra-review2 (team-lead team, agent infra-review2, host m3)\n"
                f"{stamp}verdict: APPROVE head:{HEAD}", "2026-10-12T15:00:00Z")]
        for stamp, why in (("<!-- session: Daniels-M5-Studio/unattributed -->\n",
                            "its session stamp is unattributed"),
                           ("", "it has no `ghapp` session stamp")):
            with self.subTest(why=why):
                self.assertEqual(self.judged(copied(stamp), SIGNERS), "pending")
                self.assertEqual(self.posted()["description"],
                                 "awaiting reviewer verdict; 1 approval excluded: unattributed")
                result = self.post(self.answers(copied(stamp), SIGNERS))
                self.assertIn(why, result.stderr)
        self.assertEqual(self.judged(copied(STAMP + "\n"), SIGNERS), "success", "control")

    def test_an_approve_naming_the_head_is_success(self) -> None:
        trail = [self.comment(SIGNED_APPROVE.format(head=HEAD), "2026-10-12T15:00:00Z")]
        self.assertEqual(self.judged(trail, SIGNERS), "success")
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

    def test_an_approve_from_an_untrusted_account_is_pending(self) -> None:
        # A fork's author or a drive-by account cannot forge the status.
        trail = [self.comment(f"verdict: APPROVE head:{HEAD}", "2026-10-12T15:00:00Z",
                              user={"login": "drive-by", "type": "User"},
                              author_association="NONE")]
        self.assertEqual(self.judged(trail), "pending")

    def test_an_approve_of_another_head_is_pending_and_says_so(self) -> None:
        trail = [self.comment(SIGNED_APPROVE.format(head=arm.HOLD_784), "2026-10-12T15:00:00Z")]
        self.assertEqual(self.judged(trail, SIGNERS), "pending")
        self.assertEqual(self.posted()["description"],
                         "awaiting reviewer verdict; 1 approval excluded: not_exact_head")

    def test_no_verdict_at_the_head_is_pending_not_failure(self) -> None:
        self.assertEqual(self.judged([]), "pending")
        self.assertEqual(self.posted()["description"],
                         f"awaiting reviewer verdict for head {HEAD[:12]}")

    def test_a_counted_hold_stays_failure_with_its_description(self) -> None:
        self.assertEqual(self.judged(self.refused_trail()), "failure")
        self.assertIn("HOLD naming head", self.posted()["description"])

    def test_a_hold_on_one_pull_request_sharing_the_head_is_failure(self) -> None:
        # Waiting on #785 must not hide a HOLD that #784 carries.
        pull = copy.deepcopy(arm.review_fixture("shipyard-784-pull.json"))
        pull.update(state="open", number=785)
        answers = self.answers(self.refused_trail())
        answers[:0] = [
            (f"/commits/{HEAD}/pulls", {"pages": [[
                {"number": 784, "state": "open", "head": {"sha": HEAD}},
                {"number": 785, "state": "open", "head": {"sha": HEAD}}]]}),
            ("issues/785/comments", {"pages": [[]]}),
            ("pulls/785/reviews", {"pages": [[]]}),
            ("pulls/785", {"json": pull}),
        ]
        self.assertEqual(self.post(answers).returncode, 0)
        self.assertEqual(self.posted()["state"], "failure")

    def test_an_unreadable_review_is_failure_not_waiting(self) -> None:
        broken = [(needle, {"error": "gh: HTTP 502"}) if needle == "/comments"
                  else (needle, answer) for needle, answer in self.answers([])]
        result = self.post(broken)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.posted()["state"], "failure")

    def test_a_repository_that_does_not_require_review_is_success(self) -> None:
        config = "[auto_merge]\narm_requires_head_approval = false\n"
        self.assertEqual(self.judged([], config), "success")

    def test_a_comment_on_an_issue_posts_nothing(self) -> None:
        result = self.post([], event={"issue": {"number": 5}})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / "calls").exists())

    def test_a_review_judges_the_live_head_through_workflow_run(self) -> None:
        trail = [self.comment(SIGNED_APPROVE.format(head=HEAD), "2026-10-12T15:00:00Z")]
        event = {"workflow_run": {"pull_requests": [{"number": 784}], "head_sha": arm.HOLD_784}}
        result = self.post(self.answers(trail, SIGNERS), "workflow_run", event)
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
                "|| github.event.check_run.pull_requests[0].number "
                "|| github.event.workflow_run.head_sha || github.event.check_run.head_sha }}"],
                path)

    def test_a_head_two_open_pull_requests_share_needs_both_approved(self) -> None:
        trail = [self.comment(SIGNED_APPROVE.format(head=HEAD), "2026-10-12T15:00:00Z")]
        answers = self.answers(trail, SIGNERS)
        pull = copy.deepcopy(arm.review_fixture("shipyard-784-pull.json"))
        pull.update(state="open", number=785)
        answers[:0] = [
            (f"/commits/{HEAD}/pulls", {"pages": [[
                {"number": 784, "state": "open", "head": {"sha": HEAD}},
                {"number": 785, "state": "open", "head": {"sha": HEAD}}]]}),
            ("issues/785/comments", {"pages": [[]]}),
            ("pulls/785/reviews", {"pages": [[]]}),
            ("pulls/785", {"json": pull}),
        ]
        result = self.post(answers)
        self.assertEqual(result.returncode, 0, result.stderr)
        posted = self.posted()
        self.assertEqual(posted["state"], "pending", result.stdout)
        self.assertIn("PR #784: allowed", result.stdout)
        self.assertIn("PR #785: awaiting", result.stdout)
        # Control: with only #784 on the head, the same trail is success.
        self.assertEqual(self.judged(trail, SIGNERS), "success")

    def test_a_closed_pull_request_posts_nothing(self) -> None:
        answers = self.answers([])
        closed = [(needle, {"json": dict(answer["json"], state="closed")})
                  if needle == "/pulls/" else (needle, answer) for needle, answer in answers]
        result = self.post(closed)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("/statuses/", self.calls())

    def refused_trail(self) -> list[dict[str, Any]]:
        return [self.comment(f"verdict: HOLD head:{HEAD}", "2026-10-12T15:00:00Z")]

    def approved_trail(self) -> list[dict[str, Any]]:
        return [self.comment(SIGNED_APPROVE.format(head=HEAD), "2026-10-12T15:00:00Z")]

    def test_the_head_check_is_red_when_the_verdict_is_refused(self) -> None:
        # The job is the required `review-verdict` check run on the head, so a
        # refused head must fail it, not only post a failing status.
        event = {"pull_request": {"number": 784}}
        result = self.post(self.answers(self.refused_trail()), "pull_request_target", event)
        self.assertEqual(self.posted()["state"], "failure")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertNotIn("/rerun", self.calls())

    def test_the_head_check_is_green_only_when_the_verdict_is_allowed(self) -> None:
        event = {"pull_request": {"number": 784}}
        result = self.post(self.answers(self.approved_trail(), SIGNERS),
                           "pull_request_target", event)
        self.assertEqual(self.posted()["state"], "success")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_no_approval_at_all_keeps_the_head_check_red_but_the_status_pending(self) -> None:
        event = {"pull_request": {"number": 784}}
        result = self.post(self.answers([]), "pull_request_target", event)
        self.assertEqual(self.posted()["state"], "pending")
        # The required check run still fails closed; only the status reads as waiting.
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("nothing for the author to do", result.stdout)

    def test_a_comment_re_runs_the_heads_newest_run_so_its_check_follows(self) -> None:
        # A comment's own check run sits on the default branch's tip. The head's
        # check run is only updated by re-running the head's own run, newest first.
        for trail, signers in ((self.refused_trail(), None), (self.approved_trail(), SIGNERS)):
            with self.subTest(refused=signers is None):
                result = self.post(self.answers(trail, signers))
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                calls = self.calls()
                self.assertIn(f"actions/runs?head_sha={HEAD}&event=pull_request_target", calls)
                self.assertIn("-X POST repos/danielraffel/Shipyard/actions/runs/555/rerun", calls)
                self.assertNotIn("/actions/runs/500/rerun", calls)

    def test_a_held_pull_request_on_a_later_page_still_fails_the_head(self) -> None:
        # The 101st open pull request on a shared head is on page two; its HOLD
        # (or missing verdict) must still decide the status.
        pull = copy.deepcopy(arm.review_fixture("shipyard-784-pull.json"))
        pull.update(state="open", number=785)
        filler = [{"number": 1000 + n, "state": "closed", "head": {"sha": HEAD}}
                  for n in range(99)]
        answers = self.answers(self.approved_trail(), SIGNERS)
        answers[:0] = [
            (f"/commits/{HEAD}/pulls", {"pages": [
                [{"number": 784, "state": "open", "head": {"sha": HEAD}}, *filler],
                [{"number": 785, "state": "open", "head": {"sha": HEAD}}]]}),
            ("issues/785/comments", {"pages": [[]]}),
            ("pulls/785/reviews", {"pages": [[]]}),
            ("pulls/785", {"json": pull}),
        ]
        result = self.post(answers, "pull_request_target", {"pull_request": {"number": 784}})
        self.assertIn("PR #785", result.stdout, result.stdout + result.stderr)
        self.assertNotEqual(self.posted()["state"], "success")
        self.assertNotEqual(result.returncode, 0)

    def test_the_heads_own_run_on_a_later_page_is_still_re_run(self) -> None:
        # The head's pull_request_target run listed after the first 100 must
        # still be found and re-run, or a refusal leaves its check green.
        other = [{"id": 2000 + n, "name": "Other", "event": "pull_request_target"}
                 for n in range(100)]
        answers = [
            ("actions/runs?head_sha", {"pages": [
                {"total_count": 101, "workflow_runs": other},
                {"total_count": 101, "workflow_runs": [
                    {"id": 555, "name": "Review verdict", "event": "pull_request_target"}]}]})
            if needle == "actions/runs?head_sha" else (needle, answer)
            for needle, answer in self.answers(self.refused_trail())]
        result = self.post(answers)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("-X POST repos/danielraffel/Shipyard/actions/runs/555/rerun", self.calls())

    def test_a_malformed_pull_row_fails_closed_instead_of_being_dropped(self) -> None:
        answers = self.answers(self.approved_trail(), SIGNERS)
        answers = [
            (needle, {"pages": [[
                {"number": 784, "state": "open", "head": {"sha": HEAD}},
                {},
            ]]}) if needle == f"/commits/{HEAD}/pulls" else (needle, answer)
            for needle, answer in answers
        ]
        result = self.post(answers)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("pull requests on", result.stderr)
        self.assertNotIn("/statuses/", self.calls())

    def test_a_malformed_workflow_run_fails_closed_before_refresh(self) -> None:
        answers = self.answers(self.refused_trail())
        answers = [
            (needle, {"pages": [{"total_count": 3, "workflow_runs": [
                {"id": 555, "name": "Review verdict", "event": "pull_request_target"},
                {},
                {"id": 600, "name": "Other", "event": "pull_request_target"},
            ]}]}) if needle == "actions/runs?head_sha" else (needle, answer)
            for needle, answer in answers
        ]
        result = self.post(answers)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("workflow runs on", result.stderr)
        self.assertNotIn("/actions/runs/555/rerun", self.calls())

    def test_an_incomplete_or_malformed_listing_fails_the_job(self) -> None:
        cases = {
            "runs short of total_count": ("actions/runs?head_sha", {"pages": [
                {"total_count": 150, "workflow_runs": [
                    {"id": 555, "name": "Review verdict", "event": "pull_request_target"}]}]}),
            "runs page without total_count": ("actions/runs?head_sha", {"pages": [
                {"workflow_runs": []}]}),
            "runs page not an object": ("actions/runs?head_sha", {"pages": [[]]}),
            "pulls page not a list": (f"/commits/{HEAD}/pulls", {"pages": [
                {"number": 784}]}),
            "pulls page unreadable": (f"/commits/{HEAD}/pulls", {"error": "gh: HTTP 502"}),
        }
        for label, replacement in cases.items():
            with self.subTest(label):
                answers = [replacement if needle == replacement[0] else (needle, answer)
                           for needle, answer in self.answers(self.refused_trail())]
                result = self.post(answers)
                self.assertNotEqual(result.returncode, 0, label + ": " + result.stdout)
                self.assertNotIn("actions/runs/555/rerun", self.calls(), label)
                # A deliberate refusal that names the listing, not a crash.
                self.assertRegex(result.stderr, r"are (unreadable|incomplete)|cannot read live PR state", label)

    def test_a_read_error_fails_the_job_instead_of_passing(self) -> None:
        for event_name, event in (("pull_request_target", {"pull_request": {"number": 784}}),
                                  ("issue_comment", None)):
            with self.subTest(event=event_name):
                broken = [(needle, {"error": "gh: HTTP 502"}) if needle == "/pulls/"
                          else (needle, answer)
                          for needle, answer in self.answers(self.approved_trail(), SIGNERS)]
                result = self.post(broken, event_name, event)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertNotIn("/statuses/", self.calls())

    def test_a_failed_status_post_fails_the_job(self) -> None:
        broken = [(needle, {"error": "gh: HTTP 403"}) if needle == "/statuses/"
                  else (needle, answer)
                  for needle, answer in self.answers(self.approved_trail(), SIGNERS)]
        result = self.post(broken, "pull_request_target", {"pull_request": {"number": 784}})
        self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_a_refresh_that_cannot_be_requested_fails_the_job(self) -> None:
        # A stale green on the head would let a refused head merge.
        broken = [(needle, {"error": "gh: HTTP 403"}) if needle == "/actions/runs/555/rerun"
                  else (needle, answer)
                  for needle, answer in self.answers(self.refused_trail())]
        self.assertNotEqual(self.post(broken).returncode, 0)

    def test_an_unreadable_event_fails_the_job(self) -> None:
        self.post([])
        (self.root / "event.json").write_text("{not json", encoding="utf-8")
        environment = {"PATH": "/usr/bin:/bin", "GITHUB_EVENT_NAME": "issue_comment",
                       "GITHUB_EVENT_PATH": str(self.root / "event.json"),
                       "GITHUB_REPOSITORY": REPO}
        result = subprocess.run([sys.executable, str(self.scripts / STATUS.name)],
                                env=environment, capture_output=True, text=True,
                                check=False, stdin=subprocess.DEVNULL)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("error", result.stderr)

    def test_review_verdict_cannot_be_manually_dispatched(self) -> None:
        root = STATUS.parent.parent
        paths = (root / ".github/workflows/review-verdict.yml",
                 root / "src/governance_onboard.rs",
                 root / "docs/review-verdict-status.md")
        for path in paths:
            content = path.read_text(encoding="utf-8")
            self.assertNotIn("workflow_dispatch", content, path)
            self.assertNotIn("Refuse dispatch outside the default branch", content, path)
        workflow = (root / ".github/workflows/review-verdict.yml").read_text(encoding="utf-8")
        self.assertIn("pull_request_target:", workflow)
        self.assertIn("workflow_run:", workflow)

    def test_the_workflow_can_re_run_and_still_runs_on_verdicts(self) -> None:
        root = STATUS.parent.parent / ".github" / "workflows"
        workflow = (root / "review-verdict.yml").read_text(encoding="utf-8")
        for needed in ("pull_request_target:",
                       "issue_comment:", "workflow_run:",
                       "actions: write", "scripts/review_verdict_status.py"):
            self.assertIn(needed, workflow)
        self.assertNotIn("continue-on-error", workflow)
        self.assertNotIn("|| true", workflow)
        trigger = (root / "review-verdict-trigger.yml").read_text(encoding="utf-8")
        self.assertIn("pull_request_review:", trigger)


if __name__ == "__main__":
    unittest.main()


# A pull request's own workflow token can edit or delete any comment and keep
# its author (merge-queue-rehearsal#47, runs 45528887180 and 45529945081).
HOLD_BODY = f"Findings.\nreviewer: pulp-review\n{STAMP}\nverdict: HOLD head:{HEAD}"
OPT_OUT = "[auto_merge]\narm_requires_head_approval = false\n"


class TamperedVerdicts(unittest.TestCase):
    """Edited and deleted verdict comments, and a chosen base."""

    install = ReviewVerdictStatus.install
    case = ReviewVerdictStatus.case
    run_guard = ReviewVerdictStatus.run_guard
    calls = ReviewVerdictStatus.calls
    setUp = ReviewVerdictStatus.setUp
    permissive_guards = ReviewVerdictStatus.permissive_guards
    answers = ReviewVerdictStatus.answers
    comment = ReviewVerdictStatus.comment
    post = ReviewVerdictStatus.post
    posted = ReviewVerdictStatus.posted
    judged = ReviewVerdictStatus.judged

    def approved(self) -> list[dict[str, Any]]:
        return [self.comment(SIGNED_APPROVE.format(head=HEAD), "2026-10-12T15:00:00Z")]

    def statuses(self) -> list[str]:
        return [line for line in self.calls().splitlines() if f"statuses/{HEAD} " in line
                or line.endswith(f"statuses/{HEAD}")]

    def comment_event(self, action: str, body: str, before: str | None = None) -> dict[str, Any]:
        comment = self.comment(body, "2026-10-12T16:00:00Z", id=9)
        event: dict[str, Any] = {"action": action, "comment": comment,
                                 "issue": {"number": 784, "pull_request": {"url": "x"}}}
        if before is not None:
            event["changes"] = {"body": {"from": before}}
        return event

    def test_an_edited_approve_is_not_success(self) -> None:
        self.assertEqual(self.judged(self.approved(), SIGNERS), "success")  # control
        trail = [self.comment(SIGNED_APPROVE.format(head=HEAD), "2026-10-12T15:00:00Z",
                              updated_at="2026-10-12T15:00:23Z")]
        self.assertEqual(self.judged(trail, SIGNERS), "pending")
        self.assertIn("edited", self.posted()["description"])

    def test_an_approve_edited_in_its_own_second_is_not_success(self) -> None:
        # REST times have one-second resolution: an edit in the second the
        # comment was posted leaves updated_at == created_at. GraphQL's
        # lastEditedAt still shows it.
        edited = [("lastEditedAt", {"json": {"data": {"node": {
            "lastEditedAt": "2026-10-12T15:00:00Z"}}}}), *self.answers(self.approved(), SIGNERS)]
        result = self.post(edited)
        self.assertEqual(self.posted()["state"], "pending", result.stdout + result.stderr)
        partial = [("lastEditedAt", {"json": {"data": {"node": {"lastEditedAt": None}},
                                              "errors": [{"message": "rate limited"}]}}),
                   *self.answers(self.approved(), SIGNERS)]
        result = self.post(partial)
        self.assertEqual(self.posted()["state"], "failure", "errors beside partial data")
        unreadable = [("lastEditedAt", {"error": "gh: HTTP 502"}),
                      *self.answers(self.approved(), SIGNERS)]
        result = self.post(unreadable)
        self.assertEqual(self.posted()["state"], "failure", result.stdout + result.stderr)
        guard = self.run_guard(["pr", "merge", "784", "--merge", "--match-head-commit", HEAD],
                               edited, REPO, SHIPYARD_SIGNERS_FROM_BASE="1")
        self.assertNotEqual(guard.returncode, 0, guard.stderr)

    def test_a_deleted_hold_still_holds(self) -> None:
        # Control: the deleted comment was not a HOLD, so the APPROVE decides.
        quiet = self.post(self.answers(self.approved(), SIGNERS),
                          event=self.comment_event("deleted", "thanks"))
        self.assertEqual(self.posted()["state"], "success", quiet.stderr)
        result = self.post(self.answers(self.approved(), SIGNERS),
                           event=self.comment_event("deleted", HOLD_BODY))
        posted = self.posted()
        self.assertEqual(posted["state"], "failure", result.stdout + result.stderr)
        self.assertTrue(posted["description"].startswith(f"a HOLD naming head {HEAD[:12]}"),
                        posted)

    def test_a_hold_deleted_before_its_own_created_run_reads_still_holds(self) -> None:
        # A pull request's GITHUB_TOKEN deletes the HOLD at once; that delete
        # starts no workflow run, so the HOLD's own `created` payload is the
        # only evidence left. The listing no longer has it.
        result = self.post(self.answers(self.approved(), SIGNERS),
                           event=self.comment_event("created", HOLD_BODY))
        posted = self.posted()
        self.assertEqual(posted["state"], "failure", result.stdout + result.stderr)
        self.assertTrue(posted["description"].startswith(f"a HOLD naming head {HEAD[:12]}"))

    def test_a_removed_hold_holds_even_before_any_approve(self) -> None:
        # With no approval the head is only pending; the removed HOLD must
        # still be recorded as a failure, or a later APPROVE would count.
        self.post(self.answers([], SIGNERS), event=self.comment_event("created", HOLD_BODY))
        self.assertEqual(self.posted()["state"], "failure")

    def test_the_payload_holds_only_a_hold_naming_this_head(self) -> None:
        other = HOLD_BODY.replace(HEAD, arm.HOLD_784)
        self.post(self.answers(self.approved(), SIGNERS),
                  event=self.comment_event("created", other))
        self.assertEqual(self.posted()["state"], "success", "a HOLD of another head")
        # An APPROVE whose prose says HOLD reaches the payload judge, which
        # takes only HOLDs.
        prose = SIGNED_APPROVE.format(head=HEAD).replace("Reviewed.", "Reviewed; no HOLD.")
        self.post(self.answers(self.approved(), SIGNERS),
                  event=self.comment_event("created", prose))
        self.assertEqual(self.posted()["state"], "success", "an APPROVE in the payload")

    def test_a_hold_edited_into_an_approve_still_holds(self) -> None:
        self.post(self.answers(self.approved(), SIGNERS),
                  event=self.comment_event("edited", SIGNED_APPROVE.format(head=HEAD),
                                           before=HOLD_BODY))
        self.assertEqual(self.posted()["state"], "failure")

    def test_a_hold_this_job_posted_before_still_holds(self) -> None:
        held = {"context": "review-verdict", "state": "failure",
                "creator": {"login": "github-actions[bot]"},
                "description": f"a HOLD naming head {HEAD[:12]} makes it un-approvable"}
        self.history = [dict(held, creator={"login": "someone-else[bot]"})]
        self.assertEqual(self.judged(self.approved(), SIGNERS), "success",
                         "another creator's status is not this job's")
        self.history = [held]
        self.post(self.answers(self.approved(), SIGNERS))
        self.assertEqual(self.posted()["state"], "failure")
        # With the App configured, the App's own failure holds, and so does
        # the github-actions one: it may be a HOLD posted before the App
        # existed, or one the App leg could not post.
        app = {"REVIEW_VERDICT_APP_BOT_LOGIN": "fleet-verdict[bot]"}
        self.history = []
        self.post(self.answers(self.approved(), SIGNERS), **app)
        self.assertEqual(self.posted()["state"], "success", "control: no history")
        self.history = [dict(held, creator={"login": "someone-else[bot]"})]
        self.post(self.answers(self.approved(), SIGNERS), **app)
        self.assertEqual(self.posted()["state"], "success", "another creator's status")
        self.history = [held]
        self.post(self.answers(self.approved(), SIGNERS), **app)
        self.assertEqual(self.posted()["state"], "failure", "the Actions history still holds")
        self.history = [dict(held, creator={"login": "fleet-verdict[bot]"})]
        self.post(self.answers(self.approved(), SIGNERS), **app)
        self.assertEqual(self.posted()["state"], "failure")

    def test_a_numberless_event_still_reads_the_heads_hold_history(self) -> None:
        # A fork's workflow_run and a check_run can list no pull request; the
        # head's status history must still hold a head whose HOLD is gone.
        held = {"context": "review-verdict", "state": "failure",
                "creator": {"login": "github-actions[bot]"},
                "description": f"a HOLD naming head {HEAD[:12]} makes it un-approvable"}
        for name in ("workflow_run", "check_run"):
            event = {name: {"pull_requests": [], "head_sha": HEAD}}
            self.history = []
            result = self.post(self.answers(self.approved(), SIGNERS), name, copy.deepcopy(event))
            self.assertEqual(self.posted()["state"], "success", f"control {name}: {result.stderr}")
            self.history = [held]
            result = self.post(self.answers(self.approved(), SIGNERS), name, copy.deepcopy(event))
            self.assertEqual(self.posted()["state"], "failure", f"{name}: {result.stdout}")
            self.assertIn(f"/commits/{HEAD}/statuses", self.calls())

    def test_a_malformed_event_payload_refuses(self) -> None:
        # Read as "not a pull request", a malformed payload would exit 0 and
        # leave the required check green.
        broken = [("pull_request_target", {}), ("pull_request_target", {"pull_request": None}),
                  ("pull_request_target", {"pull_request": {"number": "784"}}),
                  ("pull_request_target", {"pull_request": {"number": True}}),
                  ("issue_comment", {"issue": None}),
                  ("issue_comment", {"issue": {"pull_request": {"url": "x"}}}),
                  ("issue_comment", {"issue": {"number": "784", "pull_request": {"url": "x"}}})]
        for name in ("workflow_run", "check_run"):
            broken += [(name, {}), (name, {name: None}),
                       (name, {name: {"pull_requests": [], "head_sha": None}}),
                       (name, {name: {"pull_requests": []}}),
                       (name, {name: {"pull_requests": "784", "head_sha": HEAD}}),
                       (name, {name: {"pull_requests": [{"number": "784"}], "head_sha": HEAD}}),
                       (name, {name: {"pull_requests": ["x"], "head_sha": HEAD}})]
        for name, event in broken:
            result = self.post(self.answers(self.approved(), SIGNERS), name, event)
            self.assertNotEqual(result.returncode, 0, f"{name} {event}: {result.stdout}")
            self.assertNotIn("state=success", self.calls() if (self.root / "calls").exists()
                             else "", event)
        # Controls: a comment on an issue still posts nothing, and a PR with
        # no readable state is unreadable, never "closed".
        self.assertEqual(self.post([], event={"issue": {"number": 5}}).returncode, 0)
        answers = self.answers(self.approved(), SIGNERS)
        pull = next(answer["json"] for needle, answer in answers if needle == "/pulls/")
        pull.pop("state")
        result = self.post(answers, "pull_request_target", {"pull_request": {"number": 784}})
        self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_a_malformed_status_in_the_history_refuses(self) -> None:
        held = {"context": "review-verdict", "state": "failure",
                "creator": {"login": "github-actions[bot]"},
                "description": f"a HOLD naming head {HEAD[:12]} makes it un-approvable"}
        # Control: a status of another context with a null description is
        # GitHub's own shape for a status posted without one.
        self.history = [dict(held, context="ci", description=None)]
        self.post(self.answers(self.approved(), SIGNERS))
        self.assertEqual(self.posted()["state"], "success", "control")
        broken = [{k: v for k, v in held.items() if k != "creator"},
                  dict(held, creator=None), dict(held, creator={}),
                  {k: v for k, v in held.items() if k != "context"},
                  dict(held, context=None),
                  {k: v for k, v in held.items() if k != "state"},
                  dict(held, state=7), dict(held, description=None),
                  dict(held, description=["x"]), "not a status"]
        for bad in broken:
            self.history = [bad]
            result = self.post(self.answers(self.approved(), SIGNERS))
            self.assertNotEqual(result.returncode, 0, f"{bad}: {result.stdout}")
            self.assertNotIn("state=success", self.calls(), bad)

    def test_a_malformed_comment_event_refuses(self) -> None:
        approve = SIGNED_APPROVE.format(head=HEAD)
        good = self.comment_event("edited", approve, before="thanks")
        self.post(self.answers(self.approved(), SIGNERS), event=good)
        self.assertEqual(self.posted()["state"], "success", "control")
        broken = []
        for key in ("comment", "action"):
            event = self.comment_event("deleted", approve)
            event.pop(key)
            broken.append(event)
        event = self.comment_event("deleted", approve)
        event["comment"] = "text"
        broken.append(event)
        event = self.comment_event("deleted", approve)
        event["comment"].pop("body")
        broken.append(event)
        event = self.comment_event("deleted", approve)
        event["comment"]["body"] = None
        broken.append(event)
        event = self.comment_event("deleted", approve)
        event["comment"]["user"] = None
        broken.append(event)
        edited = self.comment_event("edited", approve)
        broken.append(edited)
        for changes in ({"body": None}, {"body": {}}, {"body": {"from": 3}}, "x"):
            event = self.comment_event("edited", approve)
            event["changes"] = changes
            broken.append(event)
        for event in broken:
            result = self.post(self.answers(self.approved(), SIGNERS), event=event)
            self.assertNotEqual(result.returncode, 0, f"{event}: {result.stdout}")
            self.assertNotIn("state=success", self.calls(), event)

    def retargeted(self, base_config: str) -> list[tuple[str, dict[str, Any]]]:
        answers = self.answers([], SIGNERS)
        pull = next(answer["json"] for needle, answer in answers if needle == "/pulls/")
        pull["base"]["ref"] = "b0"
        return [("config.toml?ref=b0", {"text": base_config}),
                ("config.toml?ref=main", {"text": SIGNERS}), *answers]

    def test_policy_comes_from_the_default_branch_and_another_base_is_refused(self) -> None:
        # The N4 attack: branch b0 turns review off; the pull request into b0
        # carries no approval at all.
        result = self.post(self.retargeted(OPT_OUT))
        posted = self.posted()
        self.assertEqual(posted["state"], "failure", result.stdout + result.stderr)
        self.assertIn("not the default branch main", posted["description"])
        self.assertNotIn("ref=b0", self.calls())

    def test_the_host_guard_reads_policy_from_the_default_branch(self) -> None:
        guard = self.run_guard(["pr", "merge", "784", "--merge", "--match-head-commit", HEAD],
                               self.retargeted(OPT_OUT), REPO, SHIPYARD_SIGNERS_FROM_BASE="1")
        self.assertNotEqual(guard.returncode, 0, guard.stderr)
        self.assertNotIn("does not require a review", guard.stderr)
        self.assertNotIn("ref=b0", self.calls())

    def test_the_host_guard_refuses_a_head_its_hold_record_holds(self) -> None:
        # The HOLD comment is gone (a pull request's token deleted it); the
        # hold hook's record on this host still names the head.
        merge = ["pr", "merge", "784", "--merge", "--match-head-commit", HEAD]
        allowed = self.run_guard(merge, self.answers(self.approved(), SIGNERS), REPO,
                                 SHIPYARD_SIGNERS_FROM_BASE="1")
        self.assertEqual(allowed.returncode, 0, "control: " + allowed.stderr)
        record = self.guards / "holds.jsonl"
        record.write_text(json.dumps({"repo": "danielraffel/shipyard", "pr": 784,
                                      "hold_head": arm.HOLD_784, "action": "none",
                                      "at": "2026-11-24T00:00:00Z"}) + "\n"
                          , encoding="utf-8")
        other = self.run_guard(merge, self.answers(self.approved(), SIGNERS), REPO,
                               SHIPYARD_SIGNERS_FROM_BASE="1")
        self.assertEqual(other.returncode, 0, "a HOLD of another head: " + other.stderr)
        with record.open("a", encoding="utf-8") as handle:
            handle.write(json.dumps({"repo": REPO, "pr": 784, "hold_head": HEAD,
                                     "at": "2026-11-24T01:00:00Z"}) + "\n")
        held = self.run_guard(merge, self.answers(self.approved(), SIGNERS), REPO,
                              SHIPYARD_SIGNERS_FROM_BASE="1")
        self.assertNotEqual(held.returncode, 0, held.stderr)
        self.assertIn("hold record", held.stderr)

    def test_a_malformed_hold_record_refuses(self) -> None:
        merge = ["pr", "merge", "784", "--merge", "--match-head-commit", HEAD]
        record = self.guards / "holds.jsonl"
        row = {"repo": REPO, "pr": 9, "hold_head": HEAD, "at": "2026-11-24T01:00:00Z"}
        # Control: a well-formed row of another pull request is no record.
        record.write_text(json.dumps(row) + "\n", encoding="utf-8")
        control = self.run_guard(merge, self.answers(self.approved(), SIGNERS), REPO,
                                 SHIPYARD_SIGNERS_FROM_BASE="1")
        self.assertEqual(control.returncode, 0, "control: " + control.stderr)
        # A row missing, or mistyping, any field the match reads could be the
        # HOLD of this head, so it refuses rather than being skipped.
        held = dict(row, pr=784)
        shapes = [{k: v for k, v in held.items() if k != key}
                  for key in ("repo", "pr", "hold_head", "at")]
        shapes += [dict(held, repo=None), dict(held, repo=7), dict(held, repo=""),
                   dict(held, pr="784"), dict(held, pr=784.0), dict(held, pr=True),
                   dict(held, pr=None), dict(held, hold_head=None), dict(held, hold_head=""),
                   dict(held, hold_head=["x"]), dict(held, at=None), dict(held, at=5),
                   dict(held, at="")]
        for bad in ("not json\n", "[1, 2]\n", "\"text\"\n",
                    *(json.dumps(shape) + "\n" for shape in shapes)):
            record.write_text(json.dumps(row) + "\n" + bad, encoding="utf-8")
            guard = self.run_guard(merge, self.answers(self.approved(), SIGNERS), REPO,
                                   SHIPYARD_SIGNERS_FROM_BASE="1")
            self.assertNotEqual(guard.returncode, 0, bad + guard.stderr)
            self.assertIn("hold record", guard.stderr)

    def test_the_host_guard_refuses_a_pull_request_into_another_base(self) -> None:
        merge = ["pr", "merge", "784", "--merge", "--match-head-commit", HEAD]
        answers = self.answers(self.approved(), SIGNERS)
        pull = next(answer["json"] for needle, answer in answers if needle == "/pulls/")
        pull["base"]["ref"] = "b0"
        guard = self.run_guard(merge, answers, REPO, SHIPYARD_SIGNERS_FROM_BASE="1")
        self.assertNotEqual(guard.returncode, 0, guard.stderr)
        self.assertIn("not the default branch main", guard.stderr)
        self.assertNotIn("ref=b0", self.calls())

    def test_the_host_guard_refuses_another_base_even_when_review_is_off(self) -> None:
        # The default branch's policy turns review off; that opt-out still
        # never lands a pull request into another base.
        merge = ["pr", "merge", "784", "--merge", "--match-head-commit", HEAD]

        def into(base: str) -> list[tuple[str, dict[str, Any]]]:
            answers = self.answers([], OPT_OUT)
            pull = next(answer["json"] for needle, answer in answers if needle == "/pulls/")
            pull["base"]["ref"] = base
            return answers

        allowed = self.run_guard(merge, into("main"), REPO, SHIPYARD_SIGNERS_FROM_BASE="1")
        self.assertEqual(allowed.returncode, 0, "control: " + allowed.stderr)
        refused = self.run_guard(merge, into("b0"), REPO, SHIPYARD_SIGNERS_FROM_BASE="1")
        self.assertNotEqual(refused.returncode, 0, refused.stderr)
        self.assertIn("not the default branch main", refused.stderr)
        self.assertNotIn("does not require a review", refused.stderr)

    def test_an_unreadable_hold_record_refuses(self) -> None:
        (self.guards / "holds.jsonl").mkdir()
        guard = self.run_guard(["pr", "merge", "784", "--merge", "--match-head-commit", HEAD],
                               self.answers(self.approved(), SIGNERS), REPO,
                               SHIPYARD_SIGNERS_FROM_BASE="1")
        self.assertNotEqual(guard.returncode, 0, guard.stderr)
        self.assertIn("hold record", guard.stderr)

    def test_an_api_comment_with_no_node_id_never_approves(self) -> None:
        trail = [self.comment(SIGNED_APPROVE.format(head=HEAD), "2026-10-12T15:00:00Z")]
        trail[0].pop("node_id")
        result = self.post(self.answers(trail, SIGNERS))
        self.assertEqual(self.posted()["state"], "failure", result.stdout + result.stderr)
        self.assertIn("node id", result.stdout + result.stderr)

    def app_gh(self, fake: pathlib.Path) -> pathlib.Path:
        """``gh`` that refuses the App token and passes anything else to ``fake``."""
        wrapper = self.root / "gh-app"
        wrapper.write_text(
            "#!/bin/sh\n"
            'if [ "$GH_TOKEN" = broken-app-token ]; then echo "HTTP 401 Bad credentials" >&2; '
            "exit 1; fi\n"
            f'exec {fake} "$@"\n', encoding="utf-8")
        wrapper.chmod(0o755)
        return wrapper

    def test_the_app_leg_fails_open(self) -> None:
        answers = self.answers(self.approved(), SIGNERS)
        baseline = self.post(answers, event_name="pull_request_target",
                             event={"pull_request": {"number": 784}})
        self.assertEqual(baseline.returncode, 0, baseline.stderr)
        # A token that could not be minted: logged, Actions post unchanged.
        empty = self.post(answers, event_name="pull_request_target",
                          event={"pull_request": {"number": 784}},
                          REVIEW_VERDICT_APP_ID="123", REVIEW_VERDICT_APP_TOKEN="")
        self.assertEqual(empty.returncode, 0, empty.stderr)
        self.assertIn("::warning title=review-verdict App leg::", empty.stdout)
        self.assertEqual(len(self.statuses()), 1)
        # A refused App post: logged without the token, Actions post unchanged.
        self.install(answers)
        broken = self.post(answers, event_name="pull_request_target",
                           event={"pull_request": {"number": 784}},
                           GHAPP_REAL_GH=str(self.app_gh(self.root / "gh")),
                           REVIEW_VERDICT_APP_ID="123", REVIEW_VERDICT_APP_TOKEN="broken-app-token")
        self.assertEqual(broken.returncode, 0, broken.stderr)
        self.assertIn("the App post failed", broken.stdout)
        self.assertNotIn("broken-app-token", broken.stdout + broken.stderr)
        self.assertEqual(len(self.statuses()), 1)
        # A refusal the App cannot post fails the job after a retry, so the
        # App's last `success` is never left standing on a refused head.
        held = self.answers([], SIGNERS)
        self.install(held)
        refused = self.post(held, event=plain_comment_event(),
                            GHAPP_REAL_GH=str(self.app_gh(self.root / "gh")),
                            REVIEW_VERDICT_APP_ID="123",
                            REVIEW_VERDICT_APP_TOKEN="broken-app-token")
        self.assertEqual(refused.returncode, 1, refused.stdout + refused.stderr)
        self.assertEqual(refused.stdout.count("the App post failed"), 2, "one retry")
        self.assertIn("::error title=review-verdict App leg::", refused.stdout)
        self.assertEqual(len(self.statuses()), 1, "the Actions post still happens")
        # A working App token posts the same status twice.
        self.install(answers)
        good = self.post(answers, event_name="pull_request_target",
                         event={"pull_request": {"number": 784}},
                         GHAPP_REAL_GH=str(self.app_gh(self.root / "gh")),
                         REVIEW_VERDICT_APP_ID="123", REVIEW_VERDICT_APP_TOKEN="good-app-token")
        self.assertEqual(good.returncode, 0, good.stderr)
        self.assertEqual(len(self.statuses()), 2)


class CarryFromAHeldHead(unittest.TestCase):
    """The delta review's D1: APPROVE(H), HOLD(H), the HOLD deleted, the base
    merged in (H2). The approval of H carries to H2; a held head's approval
    never carries."""

    H = "a" * 40
    H2 = "b" * 40

    def setUp(self) -> None:
        import importlib.util
        spec = importlib.util.spec_from_file_location("review_verdict_status", STATUS)
        assert spec is not None and spec.loader is not None
        self.status = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.status)
        self.guard = self.status.load_guard()

    def poster_state(self, held: dict[str, str]) -> str:
        from unittest import mock
        posted: list[list[str]] = []

        def real_gh(arguments: list[str]) -> Any:
            if arguments[-1] == f"repos/{REPO}/pulls/7":
                return {"state": "open", "head": {"sha": self.H2}}
            if "/statuses/" in " ".join(arguments):
                posted.append(arguments)
                return {}
            raise AssertionError(arguments)

        self.guard.run_real_gh = real_gh
        carried = self.status.Decision(True, "PR #7: carried", None, (), self.H)
        with mock.patch.object(self.status, "load_guard", return_value=self.guard), \
                mock.patch.object(self.status, "open_pulls_on", return_value=[7]), \
                mock.patch.object(self.status, "decide", return_value=carried), \
                mock.patch.object(self.status, "refresh_head_check"), \
                mock.patch.object(self.status, "held_in_history",
                                  side_effect=lambda guard, repo, head: held.get(head)):
            self.status.run("pull_request_target", {"pull_request": {"number": 7}}, REPO, None)
        state = next(a for a in posted[-1] if a.startswith("state="))
        return state.split("=", 1)[1]

    def test_the_poster_refuses_a_carry_from_a_held_head(self) -> None:
        self.assertEqual(self.poster_state({}), "success", "control: nothing held")
        self.assertEqual(self.poster_state({self.H: "a HOLD naming head aaaa"}), "failure")

    def guard_gate(self, holds: dict[str, str | None]) -> tuple[bool, str]:
        import os
        from unittest import mock
        pull = {"base": {"ref": "main", "repo": {"default_branch": "main"}},
                "head": {"sha": self.H2, "ref": "feature/x"}, "user": {"login": "author"}}
        target = {"repo": REPO, "pr": 7, "head": self.H2, "bound_head": self.H2}
        guard = self.guard
        with tempfile.TemporaryDirectory() as guards, \
                mock.patch.dict(os.environ, {"SHIPYARD_GHAPP_GUARDS_DIR": guards}), \
                mock.patch.object(guard, "host_review_default", return_value=True), \
                mock.patch.object(guard, "run_real_gh", return_value=pull), \
                mock.patch.object(guard, "base_config_text", return_value=SIGNERS), \
                mock.patch.object(guard, "_rest_pages", return_value=[]), \
                mock.patch.object(guard, "review_verdict", return_value=(False, "no verdict")), \
                mock.patch.object(guard, "approved_earlier_head", return_value=self.H), \
                mock.patch.object(guard, "carry_limits", return_value=None), \
                mock.patch.object(guard, "merge_only_carry", return_value=None), \
                mock.patch.object(guard, "self_approval", return_value=None), \
                mock.patch.object(guard, "same_session_note", return_value=None), \
                mock.patch.object(guard, "deciding_approval", return_value={}), \
                mock.patch.object(guard, "recorded_hold",
                                  side_effect=lambda repo, number, head: holds.get(head)):
            allowed, message, _ = guard.review_gate(target, REPO, dry_run=True)
        return allowed, message

    def test_the_guard_refuses_a_carry_from_a_held_head(self) -> None:
        allowed, message = self.guard_gate({})
        self.assertTrue(allowed, "control: " + message)
        allowed, message = self.guard_gate({self.H: "2026-11-24T01:00:00Z"})
        self.assertFalse(allowed, message)
        self.assertIn("never carries", message)


class RollupMemberEdit(unittest.TestCase):
    """A rollup member's approval meets the same GraphQL edit rule."""

    def test_an_edited_member_approval_does_not_count(self) -> None:
        from unittest import mock
        guard = arm.guard

        def review(deciding: dict[str, Any], edited: str | None) -> str | None:
            with mock.patch.object(guard, "run_real_gh", return_value={"user": {"login": "a"}}), \
                    mock.patch.object(guard, "_rest_pages", return_value=[]), \
                    mock.patch.object(guard, "review_verdict", return_value=(True, "ok")), \
                    mock.patch.object(guard, "self_approval", return_value=None), \
                    mock.patch.object(guard, "deciding_approval", return_value=deciding), \
                    mock.patch.object(guard, "comment_last_edited", return_value=edited):
                return guard.member_review(REPO, 7, HEAD, {}, False)

        api = {"verdict_comment_id": 1, "verdict_node_id": "IC_1", "verdict_from_api": True}
        self.assertIsNone(review(api, None), "control: never edited")
        self.assertIn("was edited", review(api, "2026-11-24T01:00:00Z") or "")
        with self.assertRaises(guard.GuardError):
            review(dict(api, verdict_node_id=None), None)

"""Review items in the ``review-verdict`` status and the queue-arm guard.

A base that sets ``[auto_merge] review_items_check`` and
``review_items_workflow`` makes every head's review items (the exceptions its
own guards leave for the reviewer) part of the decision: the status is
``success`` only when the exact-head APPROVE that allows the head carries an
``accept-review-item: <id>`` line for every id. The ids come only from the
newest ``pull_request_target`` run of that workflow file on the exact head,
from its job of that name, in the job check run's one ``review-items``
annotation. Each case runs the status script as the workflow does, against
the same fake ``gh`` as test_review_verdict_status.py, and checks the guard
agrees.
"""

from __future__ import annotations

import copy
import pathlib
import sys
import unittest
from typing import Any

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import test_ghapp_queue_arm_guard as arm  # noqa: E402
import test_review_verdict_status as rv  # noqa: E402

HEAD = rv.HEAD
REPO = rv.REPO
STAMP = rv.STAMP
WORKFLOW = ".github/workflows/review-items.yml"
ITEMS = (rv.SIGNERS + 'review_items_check = "review-items"\n'
         f'review_items_workflow = "{WORKFLOW}"\n')
HOTSPOT = "hotspot:docs/status/consumption-profiles.json"
FANOUT = "fanout:core/audio/include/pulp/audio/buffer.hpp:+12"
OTHER = "f" * 40


def approve(*accepts: str, head: str = HEAD) -> str:
    lines = "".join(f"accept-review-item: {item}\n" for item in accepts)
    return f"Reviewed.\n{lines}reviewer: pulp-review\n{STAMP}\nverdict: APPROVE head:{head}"


def items_run(run_id: int = 700, **change: Any) -> dict[str, Any]:
    run = {"id": run_id, "name": "Review items", "event": "pull_request_target",
           "path": WORKFLOW, "head_sha": HEAD, "status": "completed", "conclusion": "success"}
    run.update(change)
    return run


def items_job(job_id: int = 9001, run_id: int = 700, **change: Any) -> dict[str, Any]:
    job = {"id": job_id, "name": "review-items", "run_id": run_id, "head_sha": HEAD,
           "status": "completed", "conclusion": "success",
           "check_run_url": f"https://api.github.com/repos/{REPO}/check-runs/{job_id}"}
    job.update(change)
    return job


def listing(*ids: str, count: int | None = None) -> dict[str, Any]:
    lines = [f"review-item: {item}" for item in ids]
    lines.append(f"review-items-count: {len(ids) if count is None else count}")
    return {"title": "review-items", "message": "\n".join(lines),
            "annotation_level": "notice", "path": ".github"}


# The `Review verdict` runs the status re-runs (refresh_head_check), as GitHub
# lists them, so they read like any other row of the same listing.
VERDICT_RUNS = [
    {"id": 500, "name": "Review verdict", "event": "pull_request_target", "path":
     ".github/workflows/review-verdict.yml", "head_sha": HEAD, "status": "completed",
     "conclusion": "failure"},
    {"id": 555, "name": "Review verdict", "event": "pull_request_target", "path":
     ".github/workflows/review-verdict.yml", "head_sha": HEAD, "status": "completed",
     "conclusion": "failure"},
]


class ReviewItemsStatus(unittest.TestCase):
    setUp = rv.ReviewVerdictStatus.setUp
    install = rv.ReviewVerdictStatus.install
    case = rv.ReviewVerdictStatus.case
    run_guard = rv.ReviewVerdictStatus.run_guard
    calls = rv.ReviewVerdictStatus.calls
    permissive_guards = rv.ReviewVerdictStatus.permissive_guards
    comment = rv.ReviewVerdictStatus.comment
    post = rv.ReviewVerdictStatus.post
    posted = rv.ReviewVerdictStatus.posted

    def answers(self, trail: list[dict[str, Any]], config: str | None = ITEMS, *,
                runs: list[dict[str, Any]] | None = None,
                jobs: list[dict[str, Any]] | None = None,
                annotations: list[dict[str, Any]] | None = None,
                extra: list[tuple[str, dict[str, Any]]] | None = None,
                ) -> list[tuple[str, dict[str, Any]]]:
        answers = rv.ReviewVerdictStatus.answers(self, trail, config)
        listed = VERDICT_RUNS + ([items_run()] if runs is None else runs)
        answers = [(needle, {"pages": [{"total_count": len(listed), "workflow_runs": listed}]})
                   if needle == "actions/runs?head_sha" else (needle, answer)
                   for needle, answer in answers]
        jobs = [items_job()] if jobs is None else jobs
        answers[:0] = [
            *(extra or []),
            ("/jobs?", {"pages": [{"total_count": len(jobs), "jobs": jobs}]}),
            ("/annotations?", {"pages": [[listing(HOTSPOT)] if annotations is None
                                         else annotations]}),
        ]
        return answers

    def judged(self, trail: list[dict[str, Any]], config: str | None = ITEMS,
               **items: Any) -> str:
        """The status posted for ``trail``, checked to agree with the guard."""
        result = self.post(self.answers(trail, config, **items))
        self.assertEqual(result.returncode, 0, result.stderr)
        posted = self.posted()
        self.assertLessEqual(len(posted["description"]), 140)
        guard = self.run_guard(["pr", "merge", "784", "--merge", "--match-head-commit", HEAD],
                               self.answers(trail, config, **items), REPO,
                               SHIPYARD_SIGNERS_FROM_BASE="1")
        self.assertEqual(posted["state"] == "success", guard.returncode == 0,
                         f"status {posted['state']} disagrees with the guard: {guard.stderr}")
        self.last = result
        return posted["state"]

    def trail(self, body: str) -> list[dict[str, Any]]:
        return [self.comment(body, "2026-10-12T15:00:00Z")]

    # -- off -----------------------------------------------------------------

    def test_unset_config_reads_nothing_and_changes_nothing(self) -> None:
        # The same answers a configured base would get, items unaccepted: with
        # no review_items_check the status is what it always was, and no
        # items run, job or annotation is read.
        self.assertEqual(self.judged(self.trail(approve()), rv.SIGNERS), "success")
        calls = self.calls()
        self.assertNotIn("/jobs", calls)
        self.assertNotIn("/annotations", calls)
        self.assertEqual(calls.count("actions/runs?head_sha"), 1, "only refresh_head_check")

    # -- accepted / unaccepted ----------------------------------------------

    def test_every_item_accepted_is_success(self) -> None:
        state = self.judged(self.trail(approve(HOTSPOT, FANOUT)),
                            annotations=[listing(HOTSPOT, FANOUT)])
        self.assertEqual(state, "success", self.last.stderr)
        self.assertIn("it accepts all 2 review items", self.last.stdout)

    def test_extra_accepts_are_ignored(self) -> None:
        self.assertEqual(self.judged(self.trail(approve(HOTSPOT, "hotspot:not/an/item"))),
                         "success")

    def test_no_items_needs_no_accept(self) -> None:
        self.assertEqual(self.judged(self.trail(approve()), annotations=[listing()]), "success")

    def test_one_unaccepted_item_waits_on_the_reviewer_and_is_named(self) -> None:
        # Only the reviewer can fix it (a new verdict), so it reads as
        # waiting on the reviewer, never as a red the author must repair.
        state = self.judged(self.trail(approve(HOTSPOT)), annotations=[listing(HOTSPOT, FANOUT)])
        self.assertEqual(state, "pending")
        self.assertEqual(self.posted()["description"],
                         f"awaiting reviewer acceptance of review items: {FANOUT}")
        self.assertIn("nothing for the author to do", self.last.stdout)

    def test_an_approve_with_no_accept_lines_is_not_success(self) -> None:
        self.assertEqual(self.judged(self.trail(approve())), "pending")
        self.assertIn(HOTSPOT, self.posted()["description"])

    def test_fenced_quoted_or_near_miss_accept_lines_do_not_count(self) -> None:
        for body in (
                f"Reviewed.\n```\naccept-review-item: {HOTSPOT}\n```\nreviewer: pulp-review\n"
                f"{STAMP}\nverdict: APPROVE head:{HEAD}",
                f"Reviewed.\n> accept-review-item: {HOTSPOT}\nreviewer: pulp-review\n{STAMP}\n"
                f"verdict: APPROVE head:{HEAD}",
                f"Reviewed.\naccept-review-item: {HOTSPOT}.json\nreviewer: pulp-review\n{STAMP}\n"
                f"verdict: APPROVE head:{HEAD}",
                f"Reviewed. accept-review-item: {HOTSPOT}\nreviewer: pulp-review\n{STAMP}\n"
                f"verdict: APPROVE head:{HEAD}"):
            with self.subTest(body=body):
                self.assertEqual(self.judged(self.trail(body)), "pending")
                self.assertIn(HOTSPOT, self.posted()["description"])

    def test_an_accept_github_renders_as_code_keeps_the_head_pending(self) -> None:
        # The Fable review of #1767's bodies, each inside a counting APPROVE.
        accept = f"accept-review-item: {HOTSPOT}"
        for fenced in (f"~~~\n```\n{accept}\n~~~", f"````\n```\n{accept}\n````",
                       f"```diff\n ```\n {accept}\n```"):
            body = f"Reviewed.\n{fenced}\nreviewer: pulp-review\n{STAMP}\nverdict: APPROVE head:{HEAD}"
            with self.subTest(fenced=fenced):
                self.assertEqual(self.judged(self.trail(body)), "pending")
                self.assertIn(HOTSPOT, self.posted()["description"])
        # Mixed fences closed properly: the accept after them counts.
        body = (f"Reviewed.\n~~~\n```\n~~~\n{accept}\nreviewer: pulp-review\n{STAMP}\n"
                f"verdict: APPROVE head:{HEAD}")
        self.assertEqual(self.judged(self.trail(body)), "success")

    def test_an_accept_from_an_approve_that_does_not_count_is_not_success(self) -> None:
        # The same counting rules as the approval: an unsigned App APPROVE
        # carrying every accept is no approval at all.
        body = approve(HOTSPOT).replace("reviewer: pulp-review\n", "")
        self.assertEqual(self.judged(self.trail(body)), "pending")
        # An older APPROVE with the accepts does not lend them to the newer
        # APPROVE that decides.
        trail = [self.comment(approve(HOTSPOT), "2026-10-12T15:00:00Z"),
                 self.comment(approve(), "2026-10-12T16:00:00Z", id=2)]
        self.assertEqual(self.judged(trail), "pending")
        self.assertIn(HOTSPOT, self.posted()["description"])

    # -- the trusted source ---------------------------------------------------

    def test_no_items_run_yet_is_pending(self) -> None:
        self.assertEqual(self.judged(self.trail(approve(HOTSPOT)), runs=[]), "pending")
        self.assertEqual(self.posted()["description"],
                         f"awaiting the review items check for head {HEAD[:12]}")

    def test_an_items_run_still_running_is_pending(self) -> None:
        for status in ("queued", "in_progress"):
            with self.subTest(status=status):
                self.assertEqual(self.judged(
                    self.trail(approve(HOTSPOT)),
                    runs=[items_run(status=status, conclusion=None)]), "pending")

    def test_the_newest_items_run_decides(self) -> None:
        # A re-run in progress beats an older finished one.
        self.assertEqual(self.judged(self.trail(approve(HOTSPOT)), runs=[
            items_run(700), items_run(701, status="in_progress", conclusion=None)]), "pending")
        # A newer failed run beats an older success.
        self.assertEqual(self.judged(self.trail(approve(HOTSPOT)), runs=[
            items_run(700), items_run(701, conclusion="failure")]), "failure")

    def test_a_failed_or_cancelled_items_run_fails(self) -> None:
        for conclusion in ("failure", "cancelled", "skipped", "neutral", None):
            with self.subTest(conclusion=conclusion):
                self.assertEqual(self.judged(self.trail(approve(HOTSPOT)),
                                             runs=[items_run(conclusion=conclusion)]), "failure")
                self.assertIn("review items refused", self.posted()["description"])

    def test_a_same_named_run_from_a_pull_request_workflow_is_ignored(self) -> None:
        # `pull_request` runs the head's own workflow file, which the pull
        # request can rewrite to list no items. Only it is on the head here.
        # The listing asks GitHub for pull_request_target runs only, and a
        # row of another event in that answer is not GitHub's answer: the
        # status refuses it rather than reading it.
        forged = items_run(800, event="pull_request")
        result = self.post(self.answers(self.trail(approve()), runs=[forged],
                                        annotations=[listing()]))
        self.assertNotEqual(self.posted()["state"], "success", result.stdout)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(f"actions/runs?head_sha={HEAD}&event=pull_request_target", self.calls())
        # The items reader on its own ignores such a row, so it never
        # stands in for the trusted run, alone or beside it and newer.
        runs = VERDICT_RUNS + [forged]
        self.assertEqual(read_items(runs)[0], "waiting")
        self.assertEqual(read_items(runs + [items_run(700)], annotations=[listing(HOTSPOT)]),
                         ("ok", [HOTSPOT]))

    def test_a_same_named_run_of_another_workflow_file_is_ignored(self) -> None:
        for path in (".github/workflows/forged.yml", ".github/workflows/review-items.yaml",
                     f"{WORKFLOW}.bak", f"x/{WORKFLOW}"):
            with self.subTest(path=path):
                self.assertEqual(self.judged(self.trail(approve()), runs=[items_run(path=path)],
                                             annotations=[listing()]), "pending")

    def test_an_items_run_on_another_head_is_ignored(self) -> None:
        self.assertEqual(self.judged(self.trail(approve()), runs=[items_run(head_sha=OTHER)],
                                     annotations=[listing()]), "pending")

    def test_a_check_run_that_is_not_the_runs_job_is_never_read(self) -> None:
        # A workflow can create a check run named `review-items` on the head
        # through the API, with any output and details URL, and GitHub may
        # file it in any Actions check suite. Only the run's own job listing
        # names the job, so the forged check run is never consulted.
        forged = {"total_count": 1, "check_runs": [{
            "id": 4242, "name": "review-items", "head_sha": HEAD, "status": "completed",
            "conclusion": "success", "app": {"id": 15368, "slug": "github-actions"},
            "details_url": "https://github.com/x/actions/runs/700/job/9001",
            "output": {"title": "review-items", "summary": "review-items-count: 0",
                       "text": "review-items-count: 0"}}]}
        state = self.judged(self.trail(approve()),
                            extra=[(f"commits/{HEAD}/check-runs", {"pages": [forged]}),
                                   ("check-runs/4242", {"pages": [[listing()]]})])
        self.assertEqual(state, "pending")
        self.assertIn(HOTSPOT, self.posted()["description"])
        self.assertNotIn("check-runs/4242", self.calls())
        self.assertNotIn(f"commits/{HEAD}/check-runs", self.calls())

    def test_the_job_must_be_exactly_one_successful_job_of_that_run(self) -> None:
        for jobs, why in (
                ([], "no job of that name"),
                ([items_job(name="other")], "a job of another name"),
                ([items_job(9001), items_job(9002)], "two jobs of that name"),
                ([items_job(conclusion="failure")], "a failed job"),
                ([items_job(conclusion="skipped")], "a skipped job")):
            with self.subTest(why=why):
                self.assertEqual(self.judged(self.trail(approve(HOTSPOT)), jobs=jobs), "failure")

    def test_a_job_from_another_run_head_or_repository_is_unreadable(self) -> None:
        for job in (items_job(run_id=999), items_job(head_sha=OTHER),
                    items_job(check_run_url="https://api.github.com/repos/x/y/check-runs/9001"),
                    items_job(check_run_url="https://example.com/check-runs/9001")):
            with self.subTest(job=job):
                self.assertEqual(self.judged(self.trail(approve(HOTSPOT)), jobs=[job]), "failure")
                self.assertIn("cannot be read", self.posted()["description"])

    def test_the_annotation_must_be_one_well_formed_list(self) -> None:
        bad = {**listing(HOTSPOT), "message": f"review-item: {HOTSPOT}"}
        for annotations, why in (
                ([], "no list"),
                ([{**listing(HOTSPOT), "title": "Review-items"}], "a near-miss title"),
                ([listing(HOTSPOT), listing()], "two lists (one appended)"),
                ([bad], "no count line"),
                ([listing(HOTSPOT, count=0)], "a count short of the ids"),
                ([listing(HOTSPOT, count=2)], "a count past the ids (truncated)"),
                ([{**listing(), "message": "review-items-count: 0\nreview-items-count: 0"}],
                 "two count lines"),
                ([{**listing(HOTSPOT), "message": f"review-item: {HOTSPOT}\nreview-item: "
                   f"{HOTSPOT}\nreview-items-count: 2"}], "a duplicate id"),
                ([{**listing(), "message": "review-item: no-colon\nreview-items-count: 1"}],
                 "an id with no kind"),
                ([{**listing(), "message": "hello\nreview-items-count: 0"}], "a stray line"),
                ([{**listing(), "message": None}], "no message")):
            with self.subTest(why=why):
                self.assertEqual(self.judged(self.trail(approve(HOTSPOT)),
                                             annotations=annotations), "failure")

    def test_unreadable_items_fail(self) -> None:
        for needle in ("/jobs?", "/annotations?", "actions/runs?head_sha"):
            with self.subTest(needle=needle):
                answers = self.answers(self.trail(approve(HOTSPOT)))
                answers.insert(0, (needle, {"error": "gh: Server Error (HTTP 502)"}))
                result = self.post(answers)
                self.assertEqual(self.posted()["state"], "failure", result.stdout)
        # A listing that falls short of its count could hide the newest run.
        answers = self.answers(self.trail(approve(HOTSPOT)))
        answers.insert(0, ("actions/runs?head_sha", {"pages": [
            {"total_count": 5, "workflow_runs": VERDICT_RUNS + [items_run()]}]}))
        self.post(answers)
        self.assertEqual(self.posted()["state"], "failure")

    def test_a_half_or_malformed_setting_refuses_every_head(self) -> None:
        for config in (
                rv.SIGNERS + 'review_items_check = "review-items"\n',
                rv.SIGNERS + f'review_items_workflow = "{WORKFLOW}"\n',
                rv.SIGNERS + 'review_items_check = "review-items"\n'
                             'review_items_workflow = "review-items.yml"\n',
                rv.SIGNERS + 'review_items_check = "review-items"\n'
                             'review_items_workflow = ".github/workflows/../x.yml"\n',
                rv.SIGNERS + 'review_items_check = ""\n'
                             f'review_items_workflow = "{WORKFLOW}"\n',
                rv.SIGNERS + 'review_items_check = 3\n'
                             f'review_items_workflow = "{WORKFLOW}"\n'):
            with self.subTest(config=config):
                self.assertEqual(self.judged(self.trail(approve(HOTSPOT)), config), "failure")

    # -- several pull requests on one head -----------------------------------

    def shared(self, other_body: str) -> list[tuple[str, dict[str, Any]]]:
        answers = self.answers(self.trail(approve(HOTSPOT)))
        pull = copy.deepcopy(arm.review_fixture("shipyard-784-pull.json"))
        pull.update(state="open", number=785)
        answers[:0] = [
            (f"/commits/{HEAD}/pulls", {"pages": [[
                {"number": 784, "state": "open", "head": {"sha": HEAD}},
                {"number": 785, "state": "open", "head": {"sha": HEAD}}]]}),
            ("issues/785/comments", {"pages": [[self.comment(
                other_body, "2026-10-12T15:30:00Z", id=785)]]}),
            ("pulls/785/reviews", {"pages": [[]]}),
            ("pulls/785", {"json": pull}),
        ]
        return answers

    def test_a_shared_head_needs_every_pull_requests_approve_to_accept(self) -> None:
        # Backfill and every event judge each open pull request on the head:
        # #784's accepting APPROVE does not cover #785's, which accepts none.
        result = self.post(self.shared(approve()))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.posted()["state"], "pending", result.stdout)
        self.assertEqual(self.posted()["description"],
                         f"awaiting reviewer acceptance of review items: {HOTSPOT}")
        self.assertIn("PR #784: allowed", result.stdout)
        self.assertIn(f"PR #785: awaiting: PR #785 ({REPO}): unaccepted review items: {HOTSPOT}",
                      result.stdout)
        # A HOLD on either pull request still fails the shared head.
        result = self.post(self.shared(f"verdict: HOLD head:{HEAD}"))
        self.assertEqual(self.posted()["state"], "failure", result.stdout)
        # Control: both accept, so the head is allowed; the items are read
        # once for the head, not once per pull request.
        result = self.post(self.shared(approve(HOTSPOT)))
        self.assertEqual(self.posted()["state"], "success", result.stdout + result.stderr)
        self.assertEqual(self.calls().count("/annotations?"), 1)

    def test_a_backfill_style_pull_request_target_run_applies_the_same_rule(self) -> None:
        event = {"pull_request": {"number": 784}}
        result = self.post(self.answers(self.trail(approve())), "pull_request_target", event)
        self.assertEqual(self.posted()["state"], "pending")
        # The required check run on the head stays red until it is allowed.
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        result = self.post(self.answers(self.trail(approve(HOTSPOT))), "pull_request_target",
                           event)
        self.assertEqual(self.posted()["state"], "success", result.stderr)
        self.assertEqual(result.returncode, 0)

    def test_the_items_runs_completion_re_judges_the_head(self) -> None:
        # Pulp lists the items workflow under `workflow_run`; that event names
        # the head, and the status follows the finished run.
        event = {"workflow_run": {"head_sha": HEAD, "pull_requests": [{"number": 784}]}}
        result = self.post(self.answers(self.trail(approve(HOTSPOT))), "workflow_run", event)
        self.assertEqual(self.posted()["state"], "success", result.stdout + result.stderr)


def read_items(runs: list[dict[str, Any]], jobs: list[dict[str, Any]] | None = None,
               annotations: list[dict[str, Any]] | None = None) -> tuple[str, Any]:
    """``read_review_items`` against fixed answers, in process."""
    guard = arm.guard
    answers = {
        "actions/runs?head_sha": [{"total_count": len(runs), "workflow_runs": runs}],
        "/jobs?": [{"total_count": 1, "jobs": jobs or [items_job()]}],
        "/annotations?": [annotations if annotations is not None else [listing()]],
    }

    def fake(arguments: list[str]) -> Any:
        return next(answer for needle, answer in answers.items() if needle in arguments[-1])

    saved = guard.run_real_gh
    guard.run_real_gh = fake
    guard._REVIEW_ITEMS_CACHE.clear()
    try:
        return guard.read_review_items(REPO, HEAD, {"check": "review-items",
                                                    "workflow": WORKFLOW})
    finally:
        guard.run_real_gh = saved
        guard._REVIEW_ITEMS_CACHE.clear()


class AcceptingBody(unittest.TestCase):
    """Which verdict's lines count: only the exact-head APPROVE comment."""

    guard = arm.guard

    def setUp(self) -> None:
        self.guard.REVIEW_CONTEXT.clear()
        self.addCleanup(self.guard.REVIEW_CONTEXT.clear)

    def comment(self, body: str, **change: Any) -> dict[str, Any]:
        value = {"id": 1, "body": body, "created_at": "2026-10-12T15:00:00Z",
                 "updated_at": "2026-10-12T15:00:00Z"}
        value.update(change)
        self.guard.REVIEW_CONTEXT["comments"] = [value]
        return value

    def body(self, approval: dict[str, Any]) -> str | None:
        return self.guard._accepting_body({"approval": approval}, HEAD)

    def test_the_exact_head_approve_comment_counts(self) -> None:
        self.comment(approve(HOTSPOT))
        self.assertEqual(self.body({"verdict_comment_id": 1, "approved_head": HEAD}),
                         approve(HOTSPOT))

    def test_a_carried_review_admission_or_rollup_approval_accepts_nothing(self) -> None:
        self.comment(approve(HOTSPOT, head=OTHER))
        for approval in ({"verdict_comment_id": 1, "approved_head": OTHER},
                         {"verdict_review_id": 7, "approved_head": HEAD},
                         {"approved_head": HEAD, "admission_run_id": 3},
                         {"approved_head": HEAD, "rollup": []}, None):
            with self.subTest(approval=approval):
                self.assertIsNone(self.body(approval))
        # The approval names another head even when the comment it points
        # at reads as this head's APPROVE: only the exact head's own counts.
        self.comment(approve(HOTSPOT))
        self.assertIsNone(self.body({"verdict_comment_id": 1, "approved_head": OTHER}))

    def test_a_reviewed_marker_or_an_edited_comment_accepts_nothing(self) -> None:
        self.comment(f"accept-review-item: {HOTSPOT}\nreviewed: {HEAD}")
        self.assertIsNone(self.body({"verdict_comment_id": 1, "approved_head": HEAD}))
        self.comment(f"accept-review-item: {HOTSPOT}\nverdict: HOLD head:{HEAD}")
        self.assertIsNone(self.body({"verdict_comment_id": 1, "approved_head": HEAD}))
        self.comment(approve(HOTSPOT), updated_at="2026-10-12T15:00:09Z")
        self.assertIsNone(self.body({"verdict_comment_id": 1, "approved_head": HEAD}))

    def test_text_github_renders_inside_a_fence_accepts_nothing(self) -> None:
        # The three bodies of the Fable review of #1767: each line toggled
        # a naive fence state and read as an acceptance.
        accept = "accept-review-item: hotspot:x"
        for body in (f"~~~\n```\n{accept}\n~~~",
                     f"````\n```\n{accept}\n````",
                     f"```diff\n ```\n {accept}\n```",
                     f"```\n{accept}",
                     f"    {accept}",
                     f"\t{accept}"):
            with self.subTest(body=body):
                self.assertEqual(self.guard.accepted_review_items(body), set())
        # Mixed fences: each closes only on its own character and length,
        # and an accept after the real close counts.
        self.assertEqual(self.guard.accepted_review_items(
            f"~~~\n```\n{accept}\n```\n~~~\naccept-review-item: hotspot:after-tilde\n"
            f"````\n```\n{accept}\n`````\n   accept-review-item: hotspot:after-ticks\n"
            f"```py\n{accept}\n``` \naccept-review-item: hotspot:after-trailing-space"),
            {"hotspot:after-tilde", "hotspot:after-ticks", "hotspot:after-trailing-space"})

    def test_the_parsers_match_pulps_grammar(self) -> None:
        self.assertEqual(self.guard.accepted_review_items(
            f"accept-review-item: {HOTSPOT}\n  accept-review-item:\t{FANOUT}  \n"
            "~~~\naccept-review-item: hotspot:fenced\n~~~\n> accept-review-item: hotspot:q\n"),
            {HOTSPOT, FANOUT})
        self.assertEqual(self.guard.parse_review_items(
            f"review-item: {HOTSPOT}\n\nreview-item: {FANOUT}\nreview-items-count: 2\n"),
            [HOTSPOT, FANOUT])
        self.assertEqual(self.guard.parse_review_items("review-items-count: 0"), [])


if __name__ == "__main__":
    unittest.main()

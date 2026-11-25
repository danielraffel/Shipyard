#!/usr/bin/env python3
"""Post the ``review-verdict`` commit status on a pull request's head.

The status is the server-side twin of ``queue-arm-guard``'s review-before-arm
rule. That guard only sees landings sent through ``ghapp``; GitHub-native
auto-merge armed before a HOLD, or a workflow that merges with its own token,
never pass through it. A required ``review-verdict`` status stops those too.

The decision is the guard's own: this script calls
``ghapp_queue_arm_guard.review_gate`` with the live head bound, so the two
cannot drift. A trusted HOLD naming the head always wins (a HOLD makes its
head un-approvable; only a new head can be approved), otherwise the newest
trusted verdict naming the head decides, and only an APPROVE naming the head, or one the
guard carries to it, is ``success``. Both carries apply here: the merge-only
carry and the rebase carry (same patch-id, same files, the base touched none
of them, each keeps its approved blob and mode, no required check skipped or
receipt-reused). The status is the repository-side rule, so it turns the
rebase carry on whatever any host's switch says. Base-config signer identity
bindings are enforced by that same guard, including the stamped host and
``agent:`` value.

It runs from a workflow triggered by ``pull_request_target``, ``issue_comment``
and ``workflow_run`` (a review reaches it through a permission-less trigger
workflow). It never checks out or runs the
pull request's code, and it reads
every comment and review from the API rather than trusting the event, so a
comment from an untrusted account counts only as the guard's rules allow.

A status belongs to a commit, but a decision belongs to a pull request, and
two open pull requests can share a head. The status is ``success`` only when
the guard allows the head for every open pull request whose head it is.

The status is ``pending`` while a head is only awaiting its reviewer (no
verdict names it yet, or only approvals that do not count do), and
``failure`` for a real refusal: a counted HOLD, or anything the guard could not
read. Branch protection treats ``pending`` as unsatisfied exactly like
``failure``, so waiting never admits; it only reads as waiting.

Exit status: 0 when the head is allowed (or there is nothing to judge), 1 when
it is refused, awaiting its reviewer, or anything could not be read or posted
(the job is the required check run, so it stays red until the head is allowed). The workflow's job is
the required ``review-verdict`` check run, so it is green only for an allowed
head. Only the ``pull_request_target`` run leaves its check run on the pull
request's head; a comment or review run (which leaves its check run on the
default branch's tip) re-runs the head's ``pull_request_target`` run instead,
so an APPROVE turns the head's check green without a new push.

Usage: ``review_verdict_status.py`` with ``GITHUB_EVENT_NAME``,
``GITHUB_EVENT_PATH``, ``GITHUB_REPOSITORY`` and ``GH_TOKEN`` set (a GitHub
Actions job), and ``gh`` on ``PATH``.
"""

from __future__ import annotations

import importlib.util
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
from types import ModuleType
from typing import Any, NamedTuple

CONTEXT = "review-verdict"
# GitHub refuses a status description over 140 characters.
DESCRIPTION_LIMIT = 140
GUARD = pathlib.Path(__file__).with_name("ghapp_queue_arm_guard.py")
WORKFLOW_NAME = "Review verdict"
# Only a ``pull_request_target`` run leaves its check run on the pull
# request's head. A comment's or review's run (``issue_comment``,
# ``workflow_run``) leaves its check run on the default branch's tip, where it
# gates nothing; those runs refresh the head's check instead (see
# ``refresh_head_check``).
HEAD_CHECK_EVENTS = frozenset({"pull_request_target"})
# The guard's refusal causes that mean "no counted verdict yet": the head has
# no verdict, or only an earlier head's approval that does not carry. Every
# other refusal (a HOLD, an unreadable review, a self-approval) is a failure.
AWAITING_CAUSES = frozenset({"unreviewed", "approval_does_not_carry", "admission_missing"})
# A base whose `[auto_merge] approval_source = "guard"` takes authorization
# from the fleet admission check run instead of comments; its completion
# reaches this workflow as a ``check_run`` event (see event_target).
RECEIPT_REFUSAL = "reviewer claim receipt refused"
# Every failure this status posts for a held head starts with this, then the
# head's first 12 characters (the guard's own HOLD message, cut by
# ``describe``). A later run reads it back from the head's status history.
HOLD_PREFIX = "a HOLD naming head "
# The creator GitHub records on a status posted with a workflow's token.
ACTIONS_BOT = "github-actions[bot]"
# The dedicated verdict App's leg (see post_as_app). The id and bot login are
# public; only the token, minted per job from a main-only environment, is not.
APP_ID_ENV = "REVIEW_VERDICT_APP_ID"
APP_TOKEN_ENV = "REVIEW_VERDICT_APP_TOKEN"
APP_BOT_ENV = "REVIEW_VERDICT_APP_BOT_LOGIN"


# The ``issue_comment`` actions GitHub sends. Any other action is a payload
# this job cannot read, never one it may skip.
COMMENT_ACTIONS = frozenset({"created", "edited", "deleted"})
# Every state the pull request REST API documents. Anything else is
# unreadable, never "not open": a closed pull request posts nothing and exits
# 0, so an unknown state read as closed would leave an earlier green status.
PULL_STATES = frozenset({"open", "closed"})
# Every state a commit status can carry. A status in any other state is
# unreadable: skipped, it could be the HOLD failure this job posted.
STATUS_STATES = frozenset({"error", "failure", "pending", "success"})


class CommentEvent(NamedTuple):
    """An ``issue_comment`` payload, validated (``comment_event``)."""

    # ``None``: a comment on an issue, nothing to judge.
    number: int | None
    action: str
    comment: dict[str, Any]
    # The posted or deleted body, and on an edit the body before it.
    bodies: tuple[str, ...]


class PullRecord(NamedTuple):
    """A ``repos/{repo}/pulls/{n}`` answer, validated (``read_pull``)."""

    state: str
    head: str
    base: str
    default_branch: str
    author: str


def _is_int(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def comment_event(event: Any) -> CommentEvent:
    """The one reader of an ``issue_comment`` payload.

    Only an issue with no ``pull_request`` key is "not a pull request"; a
    marker that is present must be GitHub's object (a dict naming its
    ``url``), so a malformed marker is never read either way. For a pull
    request the comment must carry a string body and author login, the action
    must be one GitHub sends, and every edit must carry its earlier body
    (``changes.body.from``): that body is the only record of a HOLD edited
    away. Raises ``ValueError`` for anything else."""
    if not isinstance(event, dict):
        raise ValueError("the issue_comment payload is not an object")
    issue = event.get("issue")
    if not isinstance(issue, dict):
        raise ValueError("the issue_comment payload carries no issue")
    if "pull_request" not in issue:
        return CommentEvent(None, "", {}, ())
    marker = issue["pull_request"]
    if not isinstance(marker, dict) or not isinstance(marker.get("url"), str) \
            or not marker["url"]:
        raise ValueError("the issue_comment payload's pull_request marker is malformed")
    number = issue.get("number")
    if not _is_int(number):
        raise ValueError("the issue_comment payload names no pull request number")
    comment = event.get("comment")
    action = event.get("action")
    if (not isinstance(comment, dict) or not isinstance(comment.get("body"), str)
            or not isinstance(comment.get("user"), dict)
            or not isinstance(comment["user"].get("login"), str)):
        raise ValueError("the comment event payload is malformed")
    if action not in COMMENT_ACTIONS:
        raise ValueError(f"the comment event's action {action!r} is not one GitHub sends")
    bodies = [comment["body"]]
    if action == "edited":
        changes = event.get("changes")
        body = changes.get("body") if isinstance(changes, dict) else None
        before = body.get("from") if isinstance(body, dict) else None
        if not isinstance(before, str):
            raise ValueError("the comment edit carries no readable earlier body")
        bodies.append(before)
    return CommentEvent(number, action, comment, tuple(bodies))


def read_pull(guard: ModuleType, repo: str, number: int) -> PullRecord:
    """The one reader of a pull request's REST answer: its state (one of
    ``PULL_STATES``), head sha, base ref, repository default branch and author
    login, each a non-empty string, or ``GuardError``."""
    pull = guard.run_real_gh(["api", f"repos/{repo}/pulls/{number}"])
    if not isinstance(pull, dict):
        raise guard.GuardError(f"PR #{number} is unreadable")

    def text(*path: str) -> str:
        value: Any = pull
        for key in path:
            value = value.get(key) if isinstance(value, dict) else None
        if not isinstance(value, str) or not value:
            raise guard.GuardError(f"PR #{number} is unreadable (no {'.'.join(path)})")
        return value

    state = text("state")
    if state not in PULL_STATES:
        raise guard.GuardError(f"PR #{number} is unreadable (unknown state {state!r})")
    return PullRecord(state, text("head", "sha"), text("base", "ref"),
                      text("base", "repo", "default_branch"), text("user", "login"))


class Decision(NamedTuple):
    """One pull request's review decision for the head."""

    allowed: bool
    message: str
    cause: str | None
    excluded: tuple[str, ...]
    # The earlier head an allowed approval was carried from, if any.
    carried_from: str | None = None

    @property
    def awaiting(self) -> bool:
        """Refused only because no counted verdict names the head yet."""
        return (not self.allowed and self.cause in AWAITING_CAUSES
                and RECEIPT_REFUSAL not in self.message)


def load_guard() -> ModuleType:
    spec = importlib.util.spec_from_file_location("ghapp_queue_arm_guard", GUARD)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {GUARD}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def event_target(event_name: str, event: dict[str, Any]) -> tuple[int | None, str | None]:
    """``(pull request, head sha)`` an event names; either may be ``None``.

    ``(None, None)`` means there is nothing to judge (a comment on an issue).
    A ``workflow_run`` from a fork lists no pull request, only its head.
    A payload whose fields cannot be read raises (``ValueError``): read as
    "nothing to judge", it would exit 0 and leave the check green.
    """
    if not isinstance(event, dict):
        raise ValueError(f"the {event_name} payload is not an object")
    if event_name == "pull_request_target":
        pull = event.get("pull_request")
        number = pull.get("number") if isinstance(pull, dict) else None
        if not isinstance(number, int) or isinstance(number, bool):
            raise ValueError("the pull_request_target payload names no pull request number")
        return number, None
    if event_name == "issue_comment":
        return comment_event(event).number, None
    if event_name in ("check_run", "workflow_run"):
        run = event.get(event_name)
        if not isinstance(run, dict):
            raise ValueError(f"the {event_name} payload carries no {event_name}")
        # A missing list is a numberless event: its head is still judged.
        listed = run.get("pull_requests", [])
        if not isinstance(listed, list):
            raise ValueError(f"the {event_name} payload's pull_requests is not a list")
        pulls = []
        for pull in listed:
            number = pull.get("number") if isinstance(pull, dict) else None
            if not isinstance(number, int) or isinstance(number, bool):
                raise ValueError(f"the {event_name} payload lists a malformed pull request")
            pulls.append(number)
        head = run.get("head_sha")
        if not isinstance(head, str) or not head:
            raise ValueError(f"the {event_name} payload names no head")
        return (pulls[0] if pulls else None), head
    raise ValueError(f"unsupported event {event_name!r}")


def describe(allowed: bool, message: str) -> str:
    """A status description: the guard's reason, cut to GitHub's limit."""
    reason = message.split(": ", 1)[1] if ": " in message else message
    text = ("approved: " if allowed else "") + reason
    return text if len(text) <= DESCRIPTION_LIMIT else text[: DESCRIPTION_LIMIT - 1] + "…"


def exclusion_code(reason: str) -> str:
    """A short stable code for why the guard did not count an approval."""
    if "edited after it was posted" in reason:
        return "edited"
    if "unattributed" in reason or "no `ghapp` session stamp" in reason:
        return "unattributed"
    if "reviewer_stamp_sessions" in reason:
        return "unlisted_stamp_session"
    if "non-canonical session stamp" in reason:
        return "noncanonical_stamp"
    if "a user account" in reason:
        return "not_the_app"
    if any(part in reason for part in ("signer", "reviewer:", "identity")):
        return "uncounted_signer"
    return "unlisted_human" if "(" not in reason else "uncounted"


def base_refusal(guard: ModuleType, repo: str, number: int) -> str | None:
    """Why pull request ``number`` is not judged at all: its base is not the
    repository's default branch. Rulesets protect only the default branch, so
    a pull request into any other branch could be judged there under a config
    that branch carries, then retargeted to the default branch with the
    status still standing on its head (verdict-App review N4). The status is
    never ``success`` for such a pull request; a retarget re-runs this job
    (``pull_request_target: edited``)."""
    pull = read_pull(guard, repo, number)
    base, default = pull.base, pull.default_branch
    if base != default:
        return (f"PR #{number}: its base {base} is not the default branch {default}; only a "
                f"pull request into {default} is judged")
    return None


def decide(guard: ModuleType, repo: str, number: int, head: str) -> Decision:
    """The guard's review-before-arm decision for ``head``, bound to it."""
    refused_base = base_refusal(guard, repo, number)
    if refused_base is not None:
        return Decision(False, refused_base, "wrong_base", ())
    classification: dict[str, Any] = {
        "repo": repo, "pr": number, "head": head, "bound_head": head}
    # A status is read-only: it judges a carry but never logs one.
    allowed, message, fix = guard.review_gate(classification, repo, dry_run=True)
    cause = fix.get("cause") if isinstance(fix, dict) else None
    excluded = [exclusion_code(entry.split(" (", 1)[1] if " (" in entry else entry)
                for entry in classification.get("excluded_approvals") or []]
    if cause == "approval_does_not_carry":
        excluded.append("not_exact_head")
    approved = (classification.get("approval") or {}).get("approved_head")
    carried_from = (approved.lower() if allowed and isinstance(approved, str)
                    and approved.lower() != head.lower() else None)
    return Decision(allowed, message, cause, tuple(excluded), carried_from)


def awaiting_description(head: str, excluded: list[str], admission: bool = False) -> str:
    """The ``pending`` description: waiting, and why any approval did not count."""
    if admission:
        return f"awaiting fleet-guard admission for head {head[:12]}"
    if not excluded:
        return f"awaiting reviewer verdict for head {head[:12]}"
    codes = sorted(set(excluded))
    noun = "approval" if len(excluded) == 1 else "approvals"
    text = f"awaiting reviewer verdict; {len(excluded)} {noun} excluded: {', '.join(codes)}"
    return text if len(text) <= DESCRIPTION_LIMIT else text[: DESCRIPTION_LIMIT - 1] + "…"


def read_pages(guard: ModuleType, path: str, what: str) -> list[Any]:
    """Every page of a paginated REST listing (``gh api --paginate --slurp``).

    A listing that is not a list of pages, or any page that is not an array or
    an object, raises: a partial listing can omit the pull request or run that
    decides the status, so it never stands in for the whole one."""
    pages = guard.run_real_gh(["api", "--paginate", "--slurp", path])
    if not isinstance(pages, list) or not pages:
        raise guard.GuardError(f"{what} are unreadable (no page list)")
    for page in pages:
        if not isinstance(page, (list, dict)):
            raise guard.GuardError(f"{what} are unreadable (a page is not JSON data)")
    return pages


def open_pulls_on(guard: ModuleType, repo: str, head: str) -> list[int]:
    """Every open pull request whose head is ``head``, across every page."""
    what = f"the pull requests on {head[:12]}"
    pulls: list[Any] = []
    for page in read_pages(guard, f"repos/{repo}/commits/{head}/pulls?per_page=100", what):
        if not isinstance(page, list):
            raise guard.GuardError(f"{what} are unreadable")
        for pull in page:
            if (not isinstance(pull, dict)
                    or not isinstance(pull.get("number"), int)
                    or isinstance(pull.get("number"), bool)
                    or not isinstance(pull.get("state"), str)
                    or pull["state"] not in PULL_STATES
                    or not isinstance(pull.get("head"), dict)
                    or not isinstance(pull["head"].get("sha"), str)):
                raise guard.GuardError(f"{what} contain a malformed pull request")
        pulls.extend(page)
    return sorted(
        pull["number"] for pull in pulls
        if isinstance(pull, dict) and isinstance(pull.get("number"), int)
        and pull.get("state") == "open"
        and ((pull.get("head") or {}).get("sha") or "").lower() == head.lower()
    )


def removed_holds(guard: ModuleType, repo: str, number: int, head: str,
                  event_name: str, event: dict[str, Any]) -> list[str]:
    """Counted HOLDs naming ``head`` that this comment event carries.

    Any token with ``pull-requests: write`` can delete or edit any comment and
    keep its author (merge-queue-rehearsal#47), so the comment listing alone
    cannot show a HOLD that was removed seconds after it was posted. GitHub's
    own event payload still carries the text: the posted or deleted body,
    and on an edit the body before it (``changes.body.from``). Each is
    judged by the guard's own rule for a HOLD, under the default branch's
    policy. Only HOLDs count from the payload, never an APPROVE."""
    if event_name != "issue_comment":
        return []
    # A comment event whose payload cannot be read is refused, never read as
    # "no HOLD removed": the payload is the only record of a deleted HOLD.
    try:
        parsed = comment_event(event)
    except ValueError as error:
        raise guard.GuardError(str(error)) from error
    if parsed.number is None:
        raise guard.GuardError("the comment event names no pull request")
    comment = parsed.comment
    if not any("HOLD" in body for body in parsed.bodies):
        return []
    pull = read_pull(guard, repo, number)
    author = pull.author
    policy = guard.review_policy(
        guard.base_config_text(repo, pull.default_branch), repo, True)
    holds = []
    for body in parsed.bodies:
        record = {"id": comment.get("id"), "user": comment.get("user"),
                  "author_association": comment.get("author_association"),
                  "created_at": comment.get("created_at"), "body": body}
        for _, verdict, named, what in guard.counting_verdicts(
                author, [record], [], policy):
            if verdict == "HOLD" and named == head.lower() and what not in holds:
                holds.append(what)
    return holds


def status_creators() -> set[str]:
    """The creators whose ``review-verdict`` HOLD failures hold a head:
    ``github-actions[bot]``, and the verdict App's bot too once
    ``REVIEW_VERDICT_APP_BOT_LOGIN`` names it.

    The job posts every status as ``github-actions[bot]`` and only then, when
    its token could be minted, as the App (``post_as_app``), so a HOLD
    failure posted before the App existed, or one the App leg could not post,
    is in the Actions history alone. Dropping that history would let a held
    head read as unheld. Counting it fails closed: a HOLD failure another
    workflow posts can only hold a head, never admit one."""
    app_bot = os.environ.get(APP_BOT_ENV, "").strip()
    return {ACTIONS_BOT, app_bot} if app_bot else {ACTIONS_BOT}


def held_in_history(guard: ModuleType, repo: str, head: str) -> str | None:
    """A failure this job once posted on ``head`` for a HOLD, if any.

    A HOLD makes its head un-approvable, and the head's status history keeps
    every status ever posted on it, so a HOLD later deleted or edited away
    still holds. Only statuses this job's own creators posted count."""
    what = f"the statuses on {head[:12]}"
    marker = f"{HOLD_PREFIX}{head[:12]}".lower()
    creators = status_creators()
    for page in read_pages(guard, f"repos/{repo}/commits/{head}/statuses?per_page=100", what):
        if not isinstance(page, list):
            raise guard.GuardError(f"{what} are unreadable")
        for status in page:
            # Every field the match reads must be readable: a status that
            # cannot be read could be the HOLD, so it refuses rather than
            # being skipped. GitHub stores a status posted without one with
            # a null description; this job always posts one.
            creator = status.get("creator") if isinstance(status, dict) else None
            if (not isinstance(status, dict)
                    or not isinstance(status.get("context"), str)
                    or not isinstance(status.get("state"), str)
                    or status["state"] not in STATUS_STATES
                    or not isinstance(creator, dict)
                    or not isinstance(creator.get("login"), str)
                    or not (status.get("description") is None
                            or isinstance(status.get("description"), str))):
                raise guard.GuardError(f"{what} contain a malformed status")
            description = status["description"]
            if status["context"] != CONTEXT or creator["login"] not in creators:
                continue
            if not isinstance(description, str):
                raise guard.GuardError(f"{what} contain a {CONTEXT} status with no description")
            # This job posts the HOLD text only as a failure. The same text in
            # any other state was not posted by this job's success or pending
            # path, so it holds too rather than being skipped: it can only
            # refuse more.
            if description.lower().startswith(marker):
                return description
    return None


def post_as_app(arguments: list[str]) -> bool:
    """Post the same status as the dedicated verdict App; whether it posted
    (``True`` too when no App is configured).

    A token that could not be minted or a refused post is logged as a
    warning, never raised: for a ``success`` the job goes on (a missing App
    status already leaves a bound rule unmet, so failing would only turn
    pull requests red). The caller fails the job when a refusal could not be
    posted, because the App's last status would stand on a refused head
    (``run``). The token is never printed."""
    if not os.environ.get(APP_ID_ENV, "").strip():
        return True
    token = os.environ.get(APP_TOKEN_ENV, "").strip()
    if not token:
        print(f"::warning title=review-verdict App leg::{APP_TOKEN_ENV} is empty (the token "
              "could not be minted); the App status was not posted")
        return False
    real_gh = os.environ.get("GHAPP_REAL_GH", "gh")
    try:
        completed = subprocess.run(
            [real_gh, *arguments], check=False, capture_output=True, text=True, timeout=60,
            env=dict(os.environ, GH_TOKEN=token))
    except (OSError, subprocess.TimeoutExpired) as error:
        print(f"::warning title=review-verdict App leg::the App post did not run: "
              f"{type(error).__name__}")
        return False
    if completed.returncode != 0:
        tail = completed.stderr.replace(token, "***").strip()[-200:]
        print(f"::warning title=review-verdict App leg::the App post failed "
              f"(exit {completed.returncode}): {tail}")
        return False
    print("review-verdict: posted the same status as the verdict App")
    return True


def refresh_head_check(guard: ModuleType, repo: str, head: str) -> None:
    """Re-run the head's own ``pull_request_target`` run so its check run follows
    the verdict just posted. A comment or review cannot update that check run
    from its own run, and a required check left at an old ``success`` would let
    a refused head merge. Raises when the re-run cannot be requested (the job
    fails closed); nothing to re-run is not an error, because a head with no
    check run is a missing required check, not a green one."""
    what = f"the workflow runs on {head[:12]}"
    runs: list[Any] = []
    total = 0
    for page in read_pages(
            guard, f"repos/{repo}/actions/runs?head_sha={head}&event=pull_request_target"
            "&per_page=100", what):
        rows = page.get("workflow_runs") if isinstance(page, dict) else None
        count = page.get("total_count") if isinstance(page, dict) else None
        if not isinstance(rows, list) or not isinstance(count, int) or isinstance(count, bool):
            raise guard.GuardError(f"{what} are unreadable")
        for run in rows:
            if (not isinstance(run, dict)
                    or not isinstance(run.get("id"), int)
                    or isinstance(run.get("id"), bool)
                    or not isinstance(run.get("name"), str)
                    or not isinstance(run.get("event"), str)):
                raise guard.GuardError(f"{what} contain a malformed workflow run")
        runs.extend(rows)
        total = max(total, count)
    if len(runs) < total:
        # A truncated listing can hide the head's own run, and "no run" must
        # only ever mean there is none.
        raise guard.GuardError(f"{what} are incomplete: {len(runs)} of {total} read")
    ours = [r for r in runs if isinstance(r, dict) and r.get("name") == WORKFLOW_NAME
            and isinstance(r.get("id"), int)]
    if not ours:
        print(f"review-verdict: no {WORKFLOW_NAME} run on {head}; no check to refresh")
        return
    newest = max(ours, key=lambda r: r["id"])
    guard.run_real_gh_text(["api", "-X", "POST", f"repos/{repo}/actions/runs/{newest['id']}/rerun"])
    print(f"review-verdict: re-ran run {newest['id']} so the check on {head} follows the verdict")


def run(event_name: str, event: dict[str, Any], repo: str, run_url: str | None) -> int:
    """Post the status; exit 0 only when ``head`` is allowed (or nothing to
    judge). On the event whose check run sits on the head, a refusal is exit 1,
    so the required ``review-verdict`` check is red exactly when the status is."""
    number, head = event_target(event_name, event)
    if number is None and head is None:
        print("review-verdict: not a pull request; nothing to post")
        return 0
    # The guard's own switches, pinned: review is on and the rebase carry is
    # on (this status is the repository-side rule), and no host's guards
    # directory is read.
    os.environ["SHIPYARD_REVIEW_BEFORE_ARM"] = "1"
    os.environ["SHIPYARD_REBASE_CARRY"] = "1"
    # Signers only from the base config: a runner has no host signers file,
    # and a required repository that lists none admits no App APPROVE.
    os.environ["SHIPYARD_SIGNERS_FROM_BASE"] = "1"
    os.environ["SHIPYARD_GHAPP_GUARDS_DIR"] = tempfile.mkdtemp(prefix="review-verdict-guards-")
    os.environ.setdefault("GHAPP_REAL_GH", shutil.which("gh") or "gh")
    guard = load_guard()
    if number is not None:
        try:
            pull = read_pull(guard, repo, number)
        except guard.GuardError as error:
            # A pull request with no readable state is not "closed": closed
            # posts nothing and exits 0.
            print(f"review-verdict: {error}", file=sys.stderr)
            return 1
        if pull.state != "open":
            print(f"review-verdict: PR #{number} is {pull.state}; nothing to post")
            return 0
        head = pull.head
    if not head:
        print(f"review-verdict: PR #{number} has no readable head", file=sys.stderr)
        return 1
    numbers = open_pulls_on(guard, repo, head)
    if number is not None and number not in numbers:
        numbers.append(number)
    if not numbers:
        print(f"review-verdict: no open pull request has head {head}; nothing to post")
        return 0
    decisions = [(n, decide(guard, repo, n, head)) for n in sorted(numbers)]
    refused = [(n, d) for n, d in decisions if not d.allowed]
    allowed = not refused
    # Waiting is not failing: pending only when every refusal is "no counted
    # verdict yet". A counted HOLD, an unreadable review or any other refusal
    # on any pull request sharing the head is a failure.
    awaiting = bool(refused) and all(d.awaiting for _, d in refused)
    shared = (f" (judged for PRs {', '.join(f'#{n}' for n, _ in decisions)} on this head)"
              if len(decisions) > 1 else "")
    if allowed:
        state, text = "success", describe(True, decisions[0][1].message + shared)
    elif awaiting:
        state = "pending"
        text = awaiting_description(
            head, [code for _, d in refused for code in d.excluded],
            admission=all(d.cause == "admission_missing" for _, d in refused))
    else:
        hard = next(d for _, d in refused if not d.awaiting)
        state, text = "failure", describe(False, hard.message + shared)
    if state != "failure":
        # A HOLD deleted or edited away still holds: from this event's own
        # payload, else from a failure this job posted on the head before.
        # A numberless event (a fork's ``workflow_run``, a ``check_run``)
        # still reads the head's status history; only the payload check
        # needs a pull request number.
        removed = (removed_holds(guard, repo, number, head, event_name, event)
                   if number is not None else [])
        held = (f"{HOLD_PREFIX}{head[:12]} was removed after it was posted "
                f"({removed[0]}); it still holds") if removed else held_in_history(
                    guard, repo, head)
        # A held head's approval never carries: with the HOLD comment deleted,
        # a merge of the base would otherwise carry it to this head.
        for source in sorted({d.carried_from for _, d in decisions if d.carried_from}):
            if held is not None:
                break
            source_held = held_in_history(guard, repo, source)
            if source_held is not None:
                held = (f"{HOLD_PREFIX}{head[:12]}'s approval is carried from held head "
                        f"{source[:12]} ({source_held})")
        if held is not None:
            state, allowed, awaiting = "failure", False, False
            text = held if len(held) <= DESCRIPTION_LIMIT else held[: DESCRIPTION_LIMIT - 1] + "…"
            print(f"review-verdict: head {head} stays held: {held}")
    arguments = [
        "api", "-X", "POST", f"repos/{repo}/statuses/{head}",
        "-f", f"state={state}",
        "-f", f"context={CONTEXT}",
        "-f", f"description={text}",
    ]
    if run_url:
        arguments += ["-f", f"target_url={run_url}"]
    # A refusal the App could not post would leave the App's last status (an
    # earlier `success`) standing on a refused head: retry once, then fail
    # the job after the Actions post. A `success` fails open.
    app_posted = post_as_app(arguments) or (state != "success" and post_as_app(arguments))
    guard.run_real_gh(arguments)
    for n, d in decisions:
        verdict = "allowed" if d.allowed else "awaiting" if d.awaiting else "refused"
        print(f"review-verdict: PR #{n}: {verdict}: {d.message}")
    print(f"review-verdict: {state} on {head}")
    if awaiting:
        # Not a defect: the check run stays red only so GitHub cannot read it
        # as satisfied. The author has nothing to fix.
        print(f"::notice title=review-verdict::awaiting reviewer verdict for head {head[:12]}; "
              "nothing for the author to do")
    if not app_posted and state != "success":
        print(f"::error title=review-verdict App leg::the verdict App could not post {state} on "
              f"{head}; its last status may still read success, so this job fails")
    if event_name in HEAD_CHECK_EVENTS:
        # A refusal already fails this run; a success fails open.
        return 0 if allowed else 1
    refresh_head_check(guard, repo, head)
    return 0 if app_posted or state == "success" else 1


def main() -> int:
    event_name = os.environ.get("GITHUB_EVENT_NAME", "")
    repo = os.environ.get("GITHUB_REPOSITORY", "")
    event_path = os.environ.get("GITHUB_EVENT_PATH", "")
    if not (event_name and repo and event_path):
        print("review-verdict: GITHUB_EVENT_NAME, GITHUB_REPOSITORY and GITHUB_EVENT_PATH "
              "are required", file=sys.stderr)
        return 2
    server = os.environ.get("GITHUB_SERVER_URL", "https://github.com")
    run_id = os.environ.get("GITHUB_RUN_ID")
    run_url = f"{server}/{repo}/actions/runs/{run_id}" if run_id else None
    # Fail closed: any read or post error is a failed job, never a pass.
    try:
        event = json.loads(pathlib.Path(event_path).read_text(encoding="utf-8"))
        return run(event_name, event, repo, run_url)
    except Exception as error:  # noqa: BLE001 - every failure must fail the job
        print(f"review-verdict: error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())

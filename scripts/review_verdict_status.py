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


class Decision(NamedTuple):
    """One pull request's review decision for the head."""

    allowed: bool
    message: str
    cause: str | None
    excluded: tuple[str, ...]

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
    """
    if event_name == "pull_request_target":
        number = (event.get("pull_request") or {}).get("number")
        return (number if isinstance(number, int) else None), None
    if event_name == "issue_comment":
        issue = event.get("issue") or {}
        number = issue.get("number") if issue.get("pull_request") else None
        return (number if isinstance(number, int) else None), None
    if event_name == "check_run":
        run = event.get("check_run") or {}
        pulls = [pull.get("number") for pull in run.get("pull_requests") or []
                 if isinstance(pull, dict) and isinstance(pull.get("number"), int)]
        head = run.get("head_sha") if isinstance(run.get("head_sha"), str) else None
        return (pulls[0] if pulls else None), head
    if event_name == "workflow_run":
        run = event.get("workflow_run") or {}
        pulls = [pull.get("number") for pull in run.get("pull_requests") or []
                 if isinstance(pull, dict) and isinstance(pull.get("number"), int)]
        head = run.get("head_sha") if isinstance(run.get("head_sha"), str) else None
        return (pulls[0] if pulls else None), head
    raise ValueError(f"unsupported event {event_name!r}")


def describe(allowed: bool, message: str) -> str:
    """A status description: the guard's reason, cut to GitHub's limit."""
    reason = message.split(": ", 1)[1] if ": " in message else message
    text = ("approved: " if allowed else "") + reason
    return text if len(text) <= DESCRIPTION_LIMIT else text[: DESCRIPTION_LIMIT - 1] + "…"


def exclusion_code(reason: str) -> str:
    """A short stable code for why the guard did not count an approval."""
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


def decide(guard: ModuleType, repo: str, number: int, head: str) -> Decision:
    """The guard's review-before-arm decision for ``head``, bound to it."""
    classification: dict[str, Any] = {
        "repo": repo, "pr": number, "head": head, "bound_head": head}
    # A status is read-only: it judges a carry but never logs one.
    allowed, message, fix = guard.review_gate(classification, repo, dry_run=True)
    cause = fix.get("cause") if isinstance(fix, dict) else None
    excluded = [exclusion_code(entry.split(" (", 1)[1] if " (" in entry else entry)
                for entry in classification.get("excluded_approvals") or []]
    if cause == "approval_does_not_carry":
        excluded.append("not_exact_head")
    return Decision(allowed, message, cause, tuple(excluded))


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
        pulls.extend(page)
    return sorted(
        pull["number"] for pull in pulls
        if isinstance(pull, dict) and isinstance(pull.get("number"), int)
        and pull.get("state") == "open"
        and ((pull.get("head") or {}).get("sha") or "").lower() == head.lower()
    )


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
        pull = guard.run_real_gh(["api", f"repos/{repo}/pulls/{number}"])
        if not isinstance(pull, dict):
            print(f"review-verdict: PR #{number} is unreadable", file=sys.stderr)
            return 1
        if pull.get("state") != "open":
            print(f"review-verdict: PR #{number} is {pull.get('state')}; nothing to post")
            return 0
        head = (pull.get("head") or {}).get("sha") or ""
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
    arguments = [
        "api", "-X", "POST", f"repos/{repo}/statuses/{head}",
        "-f", f"state={state}",
        "-f", f"context={CONTEXT}",
        "-f", f"description={text}",
    ]
    if run_url:
        arguments += ["-f", f"target_url={run_url}"]
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
    if event_name in HEAD_CHECK_EVENTS:
        return 0 if allowed else 1
    refresh_head_check(guard, repo, head)
    return 0


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

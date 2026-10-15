#!/usr/bin/env python3
"""Post the ``review-verdict`` commit status on a pull request's head.

The status is the server-side twin of ``queue-arm-guard``'s review-before-arm
rule. That guard only sees landings sent through ``ghapp``; GitHub-native
auto-merge armed before a HOLD, or a workflow that merges with its own token,
never pass through it. A required ``review-verdict`` status stops those too.

The decision is the guard's own: this script calls
``ghapp_queue_arm_guard.review_gate`` with the live head bound, so the two
cannot drift. The newest trusted verdict naming the head decides, a HOLD with
no readable time always wins, and only an APPROVE naming the head, or one the
guard carries to it, is ``success``. Both carries apply here: the merge-only
carry and the rebase carry (same patch-id, same files, the base touched none
of them, each keeps its approved blob and mode, no required check skipped or
receipt-reused). The status is the repository-side rule, so it turns the
rebase carry on whatever any host's switch says.

It runs from a workflow triggered by ``pull_request_target``,
``issue_comment`` and ``workflow_run`` (a review reaches it through a
permission-less trigger workflow), all of which run the default branch's
definition. It never checks out or runs the pull request's code, and it reads
every comment and review from the API rather than trusting the event, so a
comment from an untrusted account counts only as the guard's rules allow.

A status belongs to a commit, but a decision belongs to a pull request, and
two open pull requests can share a head. The status is ``success`` only when
the guard allows the head for every open pull request whose head it is.

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
from typing import Any

CONTEXT = "review-verdict"
# GitHub refuses a status description over 140 characters.
DESCRIPTION_LIMIT = 140
GUARD = pathlib.Path(__file__).with_name("ghapp_queue_arm_guard.py")


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


def decide(guard: ModuleType, repo: str, number: int, head: str) -> tuple[bool, str]:
    """The guard's review-before-arm decision for ``head``, bound to it."""
    allowed, message, _ = guard.review_gate(
        {"repo": repo, "pr": number, "head": head, "bound_head": head}, repo
    )
    return allowed, message


def open_pulls_on(guard: ModuleType, repo: str, head: str) -> list[int]:
    """Every open pull request whose head is ``head``."""
    pulls = guard.run_real_gh(["api", f"repos/{repo}/commits/{head}/pulls?per_page=100"])
    if not isinstance(pulls, list):
        raise guard.GuardError(f"the pull requests on {head[:12]} are unreadable")
    return sorted(
        pull["number"] for pull in pulls
        if isinstance(pull, dict) and isinstance(pull.get("number"), int)
        and pull.get("state") == "open"
        and ((pull.get("head") or {}).get("sha") or "").lower() == head.lower()
    )


def run(event_name: str, event: dict[str, Any], repo: str, run_url: str | None) -> int:
    number, head = event_target(event_name, event)
    if number is None and head is None:
        print("review-verdict: not a pull request; nothing to post")
        return 0
    # The guard's own switches, pinned: review is on and the rebase carry is
    # on (this status is the repository-side rule), and no host's guards
    # directory is read.
    os.environ["SHIPYARD_REVIEW_BEFORE_ARM"] = "1"
    os.environ["SHIPYARD_REBASE_CARRY"] = "1"
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
    decisions = [(n, *decide(guard, repo, n, head)) for n in sorted(numbers)]
    refused = [(n, message) for n, allowed, message in decisions if not allowed]
    allowed = not refused
    message = refused[0][1] if refused else decisions[0][2]
    if len(decisions) > 1:
        message += f" (judged for PRs {', '.join(f'#{n}' for n, _, _ in decisions)} on this head)"
    state = "success" if allowed else "failure"
    arguments = [
        "api", "-X", "POST", f"repos/{repo}/statuses/{head}",
        "-f", f"state={state}",
        "-f", f"context={CONTEXT}",
        "-f", f"description={describe(allowed, message)}",
    ]
    if run_url:
        arguments += ["-f", f"target_url={run_url}"]
    guard.run_real_gh(arguments)
    for n, ok, why in decisions:
        print(f"review-verdict: PR #{n}: {'allowed' if ok else 'refused'}: {why}")
    print(f"review-verdict: {state} on {head}")
    return 0


def main() -> int:
    event_name = os.environ.get("GITHUB_EVENT_NAME", "")
    repo = os.environ.get("GITHUB_REPOSITORY", "")
    event_path = os.environ.get("GITHUB_EVENT_PATH", "")
    if not (event_name and repo and event_path):
        print("review-verdict: GITHUB_EVENT_NAME, GITHUB_REPOSITORY and GITHUB_EVENT_PATH "
              "are required", file=sys.stderr)
        return 2
    event = json.loads(pathlib.Path(event_path).read_text(encoding="utf-8"))
    server = os.environ.get("GITHUB_SERVER_URL", "https://github.com")
    run_id = os.environ.get("GITHUB_RUN_ID")
    run_url = f"{server}/{repo}/actions/runs/{run_id}" if run_id else None
    return run(event_name, event, repo, run_url)


if __name__ == "__main__":
    sys.exit(main())

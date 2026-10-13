#!/usr/bin/env python3
"""Refuse arming or enqueuing a pull request whose live queue state makes it harmful.

Intercepted (through the App-authenticated ``ghapp`` wrapper):

* ``pr merge ... --auto``;
* plain ``pr merge <pr>`` when the PR's base branch has a merge queue (there
  a plain merge enqueues);
* ``api graphql`` documents containing ``enablePullRequestAutoMerge`` or
  ``enqueuePullRequest``, whether passed as ``-f/-F query=``, ``query=@file``
  or ``--input file``. A GraphQL body read from stdin cannot be inspected and
  is refused as ambiguous.

Each intercepted PR's live state is read with the same GraphQL shape as
``shipyard landing --pr`` and classified by the Python twin of
``src/pr_queue_state.rs``. Both implementations assert against the shared
real-response corpus in ``tests/fixtures/github/``.

Refused: ``queued``, ``armed_not_queued``, ``ejected`` with no new head since
the last removal, ``merged``, ``closed`` and anything unreadable. Allowed:
``never_armed`` and ``ejected`` after a new head.

A same-head re-enqueue is allowed only when an authority states that the
ejection does not implicate the head. There are exactly two such authorities.
GitHub is one: ``invalid_merge_commit`` says GitHub failed to build the merge
commit. The repository is the other: it may declare a batch attributor under
``[queue.attribution]`` in ``.shipyard/config.toml``, which the guard asks
about the ejecting ``merge_group`` run and which must positively certify the
head as un-implicated. With no attributor declared the guard reads nothing
extra and behaves exactly as before.

The guard deliberately does NOT infer un-implication itself from the shape of
the batch failure. "The batch failed at a non-test step, so no test failed, so
the head is innocent" is unsound: a compile error is the most common way a head
breaks a batch and produces no test-failure evidence at all. Which of a
repository's steps can be influenced by repository content is knowledge the
repository has and Shipyard does not, so Shipyard asks instead of guessing.

Shipyard's own exact-head, audited enqueue path carries an internal marker
and bypasses the guard; an operator override exists and always prints a
WARNING. Both are documented for humans in docs/ghapp-guards.md and are kept
out of refusal text.

Actor identity is deliberately not consulted: every queue mutation is
attributed to the same App actor whether Shipyard or an agent issued it.

Review before arm or merge
--------------------------

An arm, an enqueue or a direct merge (``pr merge`` on a queue-less base or with
``--admin``, REST ``PUT pulls/<n>/merge``, GraphQL ``mergePullRequest``) also
needs a reviewer verdict naming the exact head, in the repositories that
require one. The protected base's ``.shipyard/config.toml`` decides when it
sets ``[auto_merge] arm_requires_head_approval``. Otherwise the repositories in
REVIEW_REQUIRED_REPOS require one. None of it applies unless the host turns
review on: ``SHIPYARD_REVIEW_BEFORE_ARM=1`` (``0`` turns it off), or a
``review-before-arm`` file in the guards directory. With it off the guard reads
nothing for this rule, so it is inert whatever the config or Python. With it
on, a config that cannot be read or parsed refuses rather than loosening. This is the Python twin of
src/head_approval.rs; both read the policy from the base branch, never from
the head being armed.

A verdict is the last non-blank line of a pull-request comment,
``verdict: APPROVE head:<sha>`` or ``reviewed: <sha>`` (``verdict: HOLD
head:<sha>`` holds), or a review in state APPROVED / CHANGES_REQUESTED on the
head. The latest verdict naming the head decides; a HOLD with no readable time
always holds. When the base config lists ``reviewer_identities``, only those
count. Otherwise the fleet App (every agent posts as it) and accounts whose
``author_association`` is OWNER, MEMBER or COLLABORATOR count, never other
accounts (the repositories are public) and never a distinguishable pull-request
author.

The request must bind the head it lands: ``--match-head-commit <sha>``,
``expectedHeadOid``, or REST ``sha=``, equal to the head the verdict approved.

A merge-only update of an approved head carries the approval to the new head
only when every commit since is a two-parent merge whose merged parent is on
the base, and every file the pull request changes has the same blob at the new
head as at the approved head. A file the base also changed since, or any
binary change in the pull request, needs a new verdict.

``GHAPP_ALLOW_UNREVIEWED_ARM=1`` is the operator override for this rule alone;
it prints a WARNING and appends an audit line. ``GHAPP_ALLOW_QUEUE_REARM`` does
not stand in for it.
"""

from __future__ import annotations

import importlib.machinery
import importlib.util
import json
import os
import pathlib
import re
import subprocess
import sys
from datetime import datetime, timedelta, timezone
from types import ModuleType
from typing import Any


ARM_MUTATIONS = ("enablepullrequestautomerge", "enqueuepullrequest")
RETRY_HAZARD_REASONS = ("failed_checks", "merge_conflict")
SAME_HEAD_REQUEUE_ALLOWED_REASONS = ("invalid_merge_commit",)
# Removal reasons a batch attributor may speak to. `merge_conflict` is absent on
# purpose: a conflict is a property of the head against its base, so it
# implicates the head no matter why the batch's checks failed.
BATCH_ATTRIBUTABLE_REASONS = ("failed_checks",)
# The only verdicts that certify a head. A verdict naming what was *not* found
# ("no test failure", "unknown") is not a certification; see the docstring.
CERTIFYING_VERDICTS = ("infrastructure", "other_pull_request")
ATTRIBUTOR_TIMEOUT_SECONDS = 120
# The ejecting run is searched by creation time around the removal, not by
# recency: on a busy queue every workflow of every failed group is its own run,
# so "the latest N failed runs" covers only the last hour or so.
MERGE_GROUP_RUN_WINDOW = timedelta(hours=6)
MERGE_GROUP_RUN_SKEW = timedelta(minutes=5)
MERGE_GROUP_RUN_PAGE = 100
MERGE_GROUP_RUN_MAX_PAGES = 3
# How much of a failed attributor's stderr a refusal quotes: enough to carry a
# traceback's final exception line, never a whole log.
ATTRIBUTOR_STDERR_TAIL_LINES = 5
ATTRIBUTOR_STDERR_TAIL_CHARS = 600
MERGE_GROUP_ANCESTRY_PROBES = 3
# Repositories that require a reviewer verdict on the exact head before arming
# when their base config does not say. Kept equal to REVIEW_REQUIRED_REPOS in
# src/head_approval.rs (a test reads both).
REVIEW_REQUIRED_REPOS = (
    "Generous-Corp/pulp",
    "Generous-Corp/vellum",
    "danielraffel/tartci",
    "danielraffel/Shipyard",
)
FLEET_AUTHOR = "shipyard-local"
REVIEW_OVERRIDE_ENV = "GHAPP_ALLOW_UNREVIEWED_ARM"
HOST_ENABLE_ENV = "SHIPYARD_REVIEW_BEFORE_ARM"
HOST_ENABLE_FILE = "review-before-arm"
TRUSTED_ASSOCIATIONS = ("OWNER", "MEMBER", "COLLABORATOR")
MERGE_MUTATIONS = ("mergepullrequest",)
# Same pattern as src/head_approval.rs marker_verdict.
_MARKER = re.compile(
    r"^[ \t]*(?:reviewed:[ \t]*([0-9a-f]{40})"
    r"|verdict:[ \t]*(APPROVE|HOLD)[ \t]+head:[ \t]*([0-9a-f]{40}))[ \t]*$"
)
OPERATOR_NOTE = (
    "An explicit authority override exists for operators; see docs/ghapp-guards.md."
)
REST_AUTO_MERGE_NOTE = (
    "REST pulls/<n>.auto_merge is null for every queued PR (GitHub consumes "
    "auto-merge on enqueue); never read it as 'unarmed'."
)

_TIMELINE = (
    "timelineItems(last:100,itemTypes:[PULL_REQUEST_COMMIT,HEAD_REF_FORCE_PUSHED_EVENT,"
    "ADDED_TO_MERGE_QUEUE_EVENT,REMOVED_FROM_MERGE_QUEUE_EVENT,AUTO_MERGE_ENABLED_EVENT,"
    "AUTO_MERGE_DISABLED_EVENT,MERGED_EVENT]){pageInfo{hasPreviousPage} nodes{__typename "
    "... on PullRequestCommit{commit{oid checkSuites(first:1){nodes{createdAt}}}} "
    "... on HeadRefForcePushedEvent{createdAt afterCommit{oid}} "
    "... on AddedToMergeQueueEvent{createdAt} "
    "... on RemovedFromMergeQueueEvent{createdAt reason beforeCommit{oid parents(first:3){nodes{oid}}}} "
    "... on AutoMergeEnabledEvent{createdAt} ... on AutoMergeDisabledEvent{createdAt} "
    "... on MergedEvent{createdAt}}}"
)
_PR_FIELDS = (
    "number state headRefOid baseRefName isInMergeQueue mergeQueueEntry{state position} "
    "autoMergeRequest{enabledAt} repository{nameWithOwner} " + _TIMELINE
)
PR_BY_NUMBER_QUERY = (
    "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name)"
    "{pullRequest(number:$number){" + _PR_FIELDS + "}}}"
)
PR_BY_ID_QUERY = "query($id:ID!){node(id:$id){... on PullRequest{" + _PR_FIELDS + "}}}"
BASE_QUEUE_QUERY = (
    "query($owner:String!,$name:String!,$base:String!){repository(owner:$owner,name:$name)"
    "{mergeQueue(branch:$base){id}}}"
)


class GuardError(RuntimeError):
    """The guard cannot prove the request is safe."""


# ---------------------------------------------------------------------------
# Request parsing: reuse the removal guard's helpers rather than a second copy.
# ---------------------------------------------------------------------------


def _load_request_parser() -> ModuleType | None:
    here = pathlib.Path(__file__).resolve().parent
    for name in ("ghapp_queue_removal_guard.py", "queue-removal-guard"):
        candidate = here / name
        if not candidate.is_file():
            continue
        loader = importlib.machinery.SourceFileLoader("_ghapp_request_parser", str(candidate))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        if spec is None:
            continue
        module = importlib.util.module_from_spec(spec)
        try:
            loader.exec_module(module)
        except Exception:  # noqa: BLE001 - any failure means "parser unavailable"
            continue
        if all(
            hasattr(module, attribute)
            for attribute in ("api_target", "graphql_document", "option_values", "GuardError")
        ):
            return module
    return None


PARSER = _load_request_parser()


# ---------------------------------------------------------------------------
# Classifier: the Python twin of src/pr_queue_state.rs.
# ---------------------------------------------------------------------------


def _pull_request(response: Any) -> dict[str, Any] | None:
    if not isinstance(response, dict):
        return None
    data = response.get("data")
    if isinstance(data, dict):
        repository = data.get("repository")
        if isinstance(repository, dict) and isinstance(repository.get("pullRequest"), dict):
            return repository["pullRequest"]
        if isinstance(data.get("node"), dict):
            return data["node"]
    if "state" in response:
        return response
    return None


def _typename(node: Any) -> str:
    return node.get("__typename", "") if isinstance(node, dict) else ""


def _oid(value: Any) -> str | None:
    oid = value.get("oid") if isinstance(value, dict) else None
    return oid if isinstance(oid, str) and oid else None


def _pushed_oid(node: Any) -> str | None:
    """The commit a new-head timeline item put on the branch."""
    typename = _typename(node)
    if typename == "PullRequestCommit":
        return _oid(node.get("commit"))
    if typename == "HeadRefForcePushedEvent":
        return _oid(node.get("afterCommit"))
    return None


def _removed_head(node: dict[str, Any], known: list[str]) -> str | None:
    """The head a ``RemovedFromMergeQueueEvent`` removed.

    ``beforeCommit`` is the merge-group commit the queue built; its second
    parent is the removed head. Accepted only when it is one of ``known``
    (commits this pull request is seen to have had); ``beforeCommit`` itself is
    accepted when it already is one of them.
    """
    before = node.get("beforeCommit")
    if not isinstance(before, dict):
        return None
    lowered = {oid.lower() for oid in known}
    oid = _oid(before)
    if oid and oid.lower() in lowered:
        return oid
    parents = before.get("parents")
    parent_nodes = parents.get("nodes") if isinstance(parents, dict) else None
    if not isinstance(parent_nodes, list):
        return None
    oids = [_oid(parent) for parent in parent_nodes]
    if len(oids) == 2 and oids[1] and oids[1].lower() in lowered:
        return oids[1]
    return None


def _same_head_re_add(
    nodes: list[Any],
    removed_heads: list[str | None],
    removal: int,
    add: int,
    queued_head: str | None,
) -> bool | None:
    """Whether the first re-add after a hazard removal re-added the removed head.

    Decided by SHA against the next removal's head, or against ``queued_head``
    (``headRefOid`` while still queued) when no removal follows. ``None`` when
    the SHAs cannot say; the caller falls back to timeline order.
    """
    removed = removed_heads[removal]
    if removed is None:
        return None
    re_added: str | None = queued_head
    for index in range(add + 1, len(nodes)):
        if _typename(nodes[index]) == "RemovedFromMergeQueueEvent":
            re_added = removed_heads[index]
            break
    if re_added is None:
        return None
    return re_added.lower() == removed.lower()


def _new_head_since(
    nodes: list[Any], index: int, removed: str | None, head: str | None
) -> tuple[bool, str]:
    """Whether ``head`` differs from the head removed at ``index``, and on what basis.

    By SHA when the removal names its head. Otherwise a force-push or commit
    after the removal (timeline order) whose oid is ``head``, or a commit of
    ``head`` whose earliest check suite was created after the removal event,
    wherever GitHub sorted it. With none of these, no new head is assumed, so a
    re-arm is refused. Commit dates are never consulted: GitHub sorts a
    back-dated commit before a removal it was pushed after. Mirrors
    ``new_head_since`` in ``src/pr_queue_state.rs``, which documents why the
    check suite stands in for the push time.
    """
    if head is None:
        return False, "no_evidence"
    if removed is not None:
        return removed.lower() != head.lower(), "removed_head_sha"
    for node in nodes[index + 1 :]:
        oid = _pushed_oid(node)
        if oid and oid.lower() == head.lower():
            return True, "push_after_removal"
    removed_at = _timestamp(nodes[index].get("createdAt"))
    if removed_at is not None:
        for node in nodes:
            if not isinstance(node, dict) or node.get("__typename") != "PullRequestCommit":
                continue
            oid = _pushed_oid(node)
            suite = _earliest_suite(node)
            if oid and oid.lower() == head.lower() and suite is not None and suite > removed_at:
                return True, "suite_after_removal"
    return False, "no_evidence"


def _timestamp(value: Any) -> datetime | None:
    if not isinstance(value, str):
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


def _earliest_suite(node: dict[str, Any]) -> datetime | None:
    """When the commit's earliest check suite was created: its push time to
    within a second, since ``Commit.pushedDate`` is null."""
    suites = (((node.get("commit") or {}).get("checkSuites") or {}).get("nodes")) or []
    times = [
        time
        for time in (_timestamp(suite.get("createdAt")) for suite in suites if isinstance(suite, dict))
        if time is not None
    ]
    return min(times) if times else None


def _ejections_of_head(
    nodes: list[Any], removed_heads: list[str | None], head: str | None
) -> int:
    """``failed_checks`` / ``merge_conflict`` removals of ``head``.

    A removal that names its head counts when that head is ``head``; one that
    names none counts once it follows the first push of ``head`` (or when no
    push of it is visible), so an unattributable removal is charged to the
    current head rather than assumed away. Mirrors
    ``PrQueueReport::ejections_of_current_head``.
    """
    if head is None:
        return 0
    first_push = next(
        (
            index
            for index, node in enumerate(nodes)
            if (_pushed_oid(node) or "").lower() == head.lower()
        ),
        None,
    )
    count = 0
    for index, node in enumerate(nodes):
        if _typename(node) != "RemovedFromMergeQueueEvent":
            continue
        reason = node.get("reason") if isinstance(node.get("reason"), str) else ""
        if reason.lower() not in RETRY_HAZARD_REASONS:
            continue
        removed = removed_heads[index]
        if removed is not None:
            charged = removed.lower() == head.lower()
        else:
            charged = first_push is None or index > first_push
        if charged:
            count += 1
    return count


def classify_pr_queue_state(response: Any) -> dict[str, Any]:
    """Classify a GraphQL pull-request response. Unreadable input is ``unknown``."""
    errors = response.get("errors") if isinstance(response, dict) else None
    if isinstance(errors, list) and errors:
        messages = "; ".join(
            str(error.get("message")) for error in errors if isinstance(error, dict)
        )
        return {"class": "unknown", "detail": f"GraphQL errors: {messages}"}
    pr = _pull_request(response)
    if pr is None:
        return {"class": "unknown", "detail": "response carries no pull request object"}
    state = pr.get("state")
    if not isinstance(state, str):
        return {"class": "unknown", "detail": "state is missing"}
    in_queue = pr.get("isInMergeQueue")
    if not isinstance(in_queue, bool):
        return {"class": "unknown", "detail": "isInMergeQueue is missing"}
    timeline = pr.get("timelineItems")
    nodes = timeline.get("nodes") if isinstance(timeline, dict) else None
    if not isinstance(nodes, list):
        return {"class": "unknown", "detail": "timelineItems.nodes is missing"}
    page_info = timeline.get("pageInfo") if isinstance(timeline, dict) else None
    has_previous = page_info.get("hasPreviousPage") if isinstance(page_info, dict) else None
    timeline_complete = (not has_previous) if isinstance(has_previous, bool) else None

    head = pr.get("headRefOid")
    head = head if isinstance(head, str) and head else None
    # Every commit this pull request is seen to have had: a removed head read
    # off a merge-group commit is trusted only when it is one of these.
    known_heads = [oid for oid in (_pushed_oid(node) for node in nodes) if oid]
    if head:
        known_heads.append(head)
    removed_heads = [
        _removed_head(node, known_heads) if _typename(node) == "RemovedFromMergeQueueEvent" else None
        for node in nodes
    ]

    requeues = 0
    # The hazard removal awaiting its first re-add, and whether a push has
    # followed it in timeline order (the fallback when SHAs cannot decide).
    hazard_pending: tuple[int, bool] | None = None
    last_removal: tuple[int, str, str | None] | None = None
    last_new_head: int | None = None
    for index, node in enumerate(nodes):
        typename = _typename(node)
        if typename in ("PullRequestCommit", "HeadRefForcePushedEvent"):
            if hazard_pending is not None:
                hazard_pending = (hazard_pending[0], True)
            last_new_head = index
        elif typename == "RemovedFromMergeQueueEvent":
            reason = node.get("reason") if isinstance(node.get("reason"), str) else "UNKNOWN"
            hazard_pending = (index, False) if reason.lower() in RETRY_HAZARD_REASONS else None
            at = node.get("createdAt") if isinstance(node.get("createdAt"), str) else None
            last_removal = (index, reason, at)
        elif typename == "AddedToMergeQueueEvent":
            if hazard_pending is not None:
                removal, pushed = hazard_pending
                same = _same_head_re_add(
                    nodes, removed_heads, removal, index, head if in_queue else None
                )
                if (not pushed) if same is None else same:
                    requeues += 1
            hazard_pending = None

    last_ejection = None
    if last_removal is not None and last_removal[1].lower() != "merged":
        index, reason, at = last_removal
        removed = removed_heads[index]
        new_head_since, basis = _new_head_since(nodes, index, removed, head)
        last_ejection = {
            "reason": reason,
            "at": at,
            "new_head_since": new_head_since,
            "removed_head": removed,
            "new_head_basis": basis,
            "merge_group_commit": _oid(nodes[index].get("beforeCommit")),
        }

    entry = pr.get("mergeQueueEntry") if isinstance(pr.get("mergeQueueEntry"), dict) else None
    auto_merge = (
        pr.get("autoMergeRequest") if isinstance(pr.get("autoMergeRequest"), dict) else None
    )
    repository = pr.get("repository")
    result: dict[str, Any] = {
        "pr": pr.get("number"),
        "head": head,
        "repo": repository.get("nameWithOwner") if isinstance(repository, dict) else None,
        "requeues_without_new_head": requeues,
        "ejections_of_current_head": _ejections_of_head(nodes, removed_heads, head),
        "last_ejection": last_ejection,
        "timeline_complete": timeline_complete,
    }
    upper = state.upper()
    if upper == "MERGED":
        result["class"] = "merged"
    elif upper == "CLOSED":
        result["class"] = "closed"
    elif upper != "OPEN":
        result.update({"class": "unknown", "detail": f"unrecognized pull request state {state}"})
    elif in_queue:
        result.update(
            {
                "class": "queued",
                "entry_state": entry.get("state") if entry else None,
                "position": entry.get("position") if entry else None,
            }
        )
    elif auto_merge is not None:
        result.update({"class": "armed_not_queued", "enabled_at": auto_merge.get("enabledAt")})
    elif last_ejection is not None:
        result.update(
            {
                "class": "ejected",
                "reason": last_ejection["reason"],
                "at": last_ejection["at"],
                "new_head_since_removal": last_ejection["new_head_since"],
            }
        )
    elif timeline_complete is False and last_removal is None and last_new_head is None:
        # A truncated window with no removal and no new head could be hiding an
        # older same-head ejection; that is not "never armed".
        result.update(
            {
                "class": "unknown",
                "detail": "timelineItems window is truncated and contains no queue removal and "
                "no new head, so an older ejection of this head cannot be ruled out",
            }
        )
    else:
        result["class"] = "never_armed"
    return result


def decide(
    classification: dict[str, Any],
    attribution: dict[str, Any] | None = None,
    environment: dict[str, Any] | None = None,
) -> tuple[bool, str]:
    """Return ``(allowed, message)`` for one classified pull request.

    ``attribution`` is a repository batch attributor's verdict about the run
    that ejected this pull request, or ``None`` when no attributor is declared,
    the ejecting run could not be resolved, or the verdict did not certify. Only
    a verdict whose ``certified`` is exactly ``True`` can turn a same-head
    refusal into an allow; every other value leaves the refusal in place.

    ``environment`` is :func:`assess_environment_requeue`'s verdict, or
    ``None`` when it does not apply. Only ``allowed`` exactly ``True`` turns a
    same-head ``failed_checks`` refusal into an allow; otherwise its reason is
    appended to the refusal so the caller sees why the one environment
    re-enqueue was not available.

    Refusal text names the correct path only. Override mechanisms are
    documented for operators in docs/ghapp-guards.md and deliberately not
    spelled out here, where an agent would read them as the next step.
    """
    number = classification.get("pr", "?")
    label = f"PR #{number}"
    land = f"`shipyard ship --pr {number}`"
    klass = classification.get("class")
    reason = str(classification.get("reason") or "unknown")
    at = classification.get("at") or "an unknown time"
    if klass == "never_armed":
        return True, f"{label} is not armed and not queued"
    if klass == "ejected" and classification.get("new_head_since_removal"):
        return True, f"{label} was removed ({reason}) and has a new head since"
    if klass == "ejected" and reason.lower() in SAME_HEAD_REQUEUE_ALLOWED_REASONS:
        return True, f"{label} was removed ({reason}); GitHub-side, nothing against this head"
    if klass == "queued":
        position = classification.get("position")
        where = f" at position {position}" if position is not None else ""
        return False, (
            f"{label} is already in the merge queue{where}; nothing to do. "
            "REST auto_merge=null is expected: " + REST_AUTO_MERGE_NOTE
        )
    if klass == "armed_not_queued":
        since = classification.get("enabled_at") or "an unknown time"
        return False, (
            f"{label} is already armed since {since}; the queue will pick it up. Nothing to do."
        )
    if klass == "ejected" and reason.lower() in RETRY_HAZARD_REASONS:
        certified = bool(attribution and attribution.get("certified") is True)
        if certified and reason.lower() in BATCH_ATTRIBUTABLE_REASONS:
            return True, (
                f"{label} was ejected for {reason} at {at}, and this repository's batch "
                f"attributor certified the ejecting batch against this head: "
                f"{attribution.get('detail', 'no detail given')}"
            )
        if (
            environment
            and environment.get("allowed") is True
            and reason.lower() == "failed_checks"
        ):
            return True, (
                f"{label} was ejected for {reason} at {at} by an environment failure or an "
                f"interruption, and this re-enqueue is allowed: {environment.get('reason')}. "
                + "; ".join(_describe_step(step) for step in environment.get("evidence") or [])
            )
        note = f" {attribution['detail']}" if attribution and attribution.get("detail") else ""
        if environment and environment.get("reason"):
            note += f" Environment re-enqueue refused: {environment['reason']}."
        return False, (
            f"{label} was ejected for {reason} at {at}; re-enqueuing the same head under "
            f"ALLGREEN fails its batch-mates. Push a fix first, then {land}. If the batch "
            f"failed on infrastructure rather than this head, give it a new head with "
            f"`gh pr update-branch {number}` (branch-refresh-guard allows that for a head the "
            f"queue removed) and then {land}.{note}"
        )
    if klass == "ejected":
        return False, (
            f"{label} was removed from the queue ({reason}) at {at} and the head has not "
            f"changed; confirm with whoever dequeued it, then give it a new head (a fix, or "
            f"`gh pr update-branch {number}`, which branch-refresh-guard allows for a head the "
            f"queue removed) and land it with {land}."
        )
    if klass in ("merged", "closed"):
        return False, f"{label} is {klass}; there is nothing to arm"
    return False, (
        f"{label} merge-queue state could not be determined "
        f"({classification.get('detail', 'unknown')}); refusing to arm blind"
    )


# ---------------------------------------------------------------------------
# Review before arm: the twin of src/head_approval.rs.
# ---------------------------------------------------------------------------


def _normalized_login(login: str) -> str:
    login = login[len("app/"):] if login.startswith("app/") else login
    login = login[: -len("[bot]")] if login.endswith("[bot]") else login
    return login.lower()


def marker_verdict(body: str) -> tuple[str, str] | None:
    """``(APPROVE|HOLD, head)`` from a comment's last non-blank line."""
    lines = [line for line in body.splitlines() if line.strip()]
    if not lines:
        return None
    match = _MARKER.match(lines[-1])
    if match is None:
        return None
    if match.group(1):
        return "APPROVE", match.group(1)
    return match.group(2), match.group(3)


def host_review_default() -> bool:
    """Whether this host applies REVIEW_REQUIRED_REPOS (see the module docstring)."""
    switch = os.environ.get(HOST_ENABLE_ENV)
    if switch == "1":
        return True
    if switch == "0":
        return False
    return (_guards_dir() / HOST_ENABLE_FILE).is_file()


def _guards_dir() -> pathlib.Path:
    return pathlib.Path(
        os.environ.get("SHIPYARD_GHAPP_GUARDS_DIR")
        or pathlib.Path.home() / ".config" / "shipyard" / "guards"
    )


def review_policy(config_text: str | None, repo: str, host_default: bool) -> dict[str, Any]:
    """``{required, reviewers, any_but_author}`` from the base's config text.

    ``config_text`` is ``None`` when the base has no config. A config that
    exists but cannot be parsed (invalid TOML, or no ``tomllib`` on this
    Python) raises GuardError: an unreadable policy never loosens the rule.
    """
    default = host_default and repo.lower() in {name.lower() for name in REVIEW_REQUIRED_REPOS}
    section: Any = None
    if config_text is not None:
        try:
            import tomllib
        except ImportError as error:
            raise GuardError(
                "the base's .shipyard/config.toml cannot be read on this Python "
                "(no tomllib; run the guard with Python 3.11 or later)"
            ) from error
        try:
            section = tomllib.loads(config_text).get("auto_merge")
        except ValueError as error:
            raise GuardError(f"the base's .shipyard/config.toml does not parse: {error}") from error
    section = section if isinstance(section, dict) else {}
    required = section.get("arm_requires_head_approval")
    reviewers = section.get("reviewer_identities", section.get("reviewer_logins")) or []
    from_default = not isinstance(required, bool) and default
    return {
        "required": required if isinstance(required, bool) else default,
        "reviewers": [str(login).lower() for login in reviewers if isinstance(login, str)],
        # Required by REVIEW_REQUIRED_REPOS, not by the repository's own key.
        "any_but_author": from_default,
    }


def _reviewer_counts(
    login: str, association: Any, policy: dict[str, Any], author: str | None
) -> bool:
    reviewer = _normalized_login(login)
    if policy.get("any_but_author") and not policy["reviewers"]:
        # The repositories are public: only the fleet App and accounts the
        # repository trusts count. Every fleet agent posts as the same App, so
        # that App's verdict on that App's pull request counts.
        trusted = reviewer == FLEET_AUTHOR or association in TRUSTED_ASSOCIATIONS
        return trusted and (
            author is None or _normalized_login(author) != reviewer or reviewer == FLEET_AUTHOR
        )
    # A repository's own setting: only its listed reviewers, never the fleet
    # App or the author (an empty list admits nobody).
    return (
        reviewer != FLEET_AUTHOR
        and any(_normalized_login(name) == reviewer for name in policy["reviewers"])
        and (author is None or _normalized_login(author) != reviewer)
    )


def counting_verdicts(
    author: str | None,
    comments: list[dict[str, Any]],
    reviews: list[dict[str, Any]],
    policy: dict[str, Any],
) -> list[tuple[datetime | None, str, str, str]]:
    """``(time, APPROVE|HOLD, head named, description)`` for every counting verdict."""
    verdicts: list[tuple[datetime | None, str, str, str]] = []
    for review in reviews:
        login = (review.get("user") or {}).get("login")
        state = str(review.get("state") or "").upper()
        verdict = {"APPROVED": "APPROVE", "CHANGES_REQUESTED": "HOLD"}.get(state)
        if not isinstance(login, str) or verdict is None:
            continue
        if _reviewer_counts(login, review.get("author_association"), policy, author):
            named = str(review.get("commit_id") or "").lower()
            verdicts.append((_timestamp(review.get("submitted_at")), verdict, named,
                             f"a {state} review by {login} at {review.get('submitted_at')}"))
    for comment in comments:
        login = (comment.get("user") or {}).get("login")
        body = comment.get("body")
        found = marker_verdict(body) if isinstance(body, str) else None
        if not isinstance(login, str) or found is None:
            continue
        if _reviewer_counts(login, comment.get("author_association"), policy, author):
            verdict, named = found
            verdicts.append((_timestamp(comment.get("created_at")), verdict, named,
                             f"`{verdict} head:{named[:12]}` by {login} at "
                             f"{comment.get('created_at')}"))
    return verdicts


def _latest_on(
    verdicts: list[tuple[datetime | None, str, str, str]], head: str
) -> tuple[datetime | None, str, str, str] | None:
    """The deciding verdict on ``head``: an untimed HOLD, else the newest."""
    on_head = [entry for entry in verdicts if entry[2] == head]
    untimed_hold = [entry for entry in on_head if entry[0] is None and entry[1] == "HOLD"]
    if untimed_hold:
        return untimed_hold[0]
    dated = [entry for entry in on_head if entry[0] is not None]
    return max(dated, key=lambda entry: entry[0]) if dated else None


def wanted_verdict(head: str, author: str | None, policy: dict[str, Any]) -> str:
    wanted = (
        f"a comment whose last line is `verdict: APPROVE head:{head}` or `reviewed: {head}`, "
        "or an APPROVED review on that commit"
    )
    if policy["reviewers"]:
        wanted += f", from one of {', '.join(policy['reviewers'])}"
    elif not policy.get("any_but_author"):
        wanted += ", from a reviewer the base config lists (it lists none)"
    else:
        wanted += ", from the fleet App or an OWNER, MEMBER or COLLABORATOR"
        if author and _normalized_login(author) != FLEET_AUTHOR:
            wanted += f" other than the PR author {author}"
    return wanted


def review_verdict(
    head: str,
    author: str | None,
    comments: list[dict[str, Any]],
    reviews: list[dict[str, Any]],
    policy: dict[str, Any],
) -> tuple[bool, str]:
    """Whether the latest counting verdict naming ``head`` approves it.

    Returns ``(allowed, why)``; a refusal says exactly what is missing.
    """
    head = head.lower()
    verdicts = counting_verdicts(author, comments, reviews, policy)
    wanted = wanted_verdict(head, author, policy)
    latest = _latest_on(verdicts, head)
    if latest is not None:
        at, verdict, _, what = latest
        if verdict == "APPROVE":
            return True, f"head {head[:12]} carries {what}"
        if at is None:
            return False, f"a HOLD naming head {head[:12]} has no readable time; it needs {wanted}"
        return False, f"the latest verdict naming head {head[:12]} is HOLD ({what}); it needs a newer {wanted}"
    older = [entry for entry in verdicts if entry[0] is not None]
    stale = ""
    if older:
        _, _, _, what = max(older, key=lambda entry: entry[0])
        stale = f" The latest verdict names another head: {what}."
    return False, f"no reviewer verdict names head {head[:12]}; it needs {wanted}.{stale}"


def approved_earlier_head(
    head: str,
    author: str | None,
    comments: list[dict[str, Any]],
    reviews: list[dict[str, Any]],
    policy: dict[str, Any],
) -> str | None:
    """The newest other head whose deciding verdict is an approval, if no verdict names ``head``."""
    head = head.lower()
    verdicts = counting_verdicts(author, comments, reviews, policy)
    if _latest_on(verdicts, head) is not None:
        return None
    floor = datetime.min.replace(tzinfo=timezone.utc)
    for _, _, named, _ in sorted(verdicts, key=lambda entry: entry[0] or floor, reverse=True):
        if named == head:
            continue
        latest = _latest_on(verdicts, named)
        return named if latest is not None and latest[1] == "APPROVE" else None
    return None


# GitHub's compare lists at most this many files; a longer list is partial.
COMPARE_FILE_CAP = 300


def _pr_files(compare: dict[str, Any], what: str) -> dict[str, tuple[str, str, bool]]:
    """``filename -> (status, blob sha, has a text patch)`` of a comparison."""
    files = compare.get("files")
    if not isinstance(files, list):
        raise GuardError(f"{what} lists no files")
    if len(files) >= COMPARE_FILE_CAP:
        raise GuardError(f"{what} lists {len(files)} files, GitHub's cap; it may be partial")
    out: dict[str, tuple[str, str, bool]] = {}
    for item in files:
        name = item.get("filename") if isinstance(item, dict) else None
        if not isinstance(name, str):
            raise GuardError(f"{what} lists a file with no name")
        out[name] = (str(item.get("status")), str(item.get("sha")), isinstance(item.get("patch"), str))
    return out


def merge_only_carry(repo: str, base: str, approved: str, head: str) -> str | None:
    """``None`` when the approval of ``approved`` carries to ``head``; else why not.

    It carries only when every commit on the first-parent chain from ``head``
    back to ``approved`` is a two-parent merge whose merged parent is on
    ``base``, and every file the pull request
    changes has, at ``head``, the same blob it had at ``approved``. A file the
    base also changed since ``approved``, or any binary change in the pull
    request, needs a new verdict: a merge could move or swap content a diff of
    the changed lines cannot see.
    """
    try:
        compare = run_real_gh(["api", f"repos/{repo}/compare/{approved}...{head}"])
        if not isinstance(compare, dict):
            return "the comparison is unreadable"
        if compare.get("status") != "ahead" or compare.get("behind_by") != 0:
            return f"{head[:12]} does not descend from {approved[:12]}"
        commits = compare.get("commits") or []
        if len(commits) != compare.get("total_commits"):
            return "the comparison lists only some of its commits"
        parents_of = {
            str(commit.get("sha")).lower(): [str(parent.get("sha")).lower()
                                             for parent in commit.get("parents") or []]
            for commit in commits
        }
        # The update's own commits are the first-parent chain from the new head
        # back to the approved one; the rest arrived through merged parents.
        sha = head.lower()
        while sha != approved.lower():
            parents = parents_of.get(sha)
            if parents is None:
                return f"commit {sha[:12]} is outside the comparison"
            if len(parents) != 2:
                return f"commit {sha[:12]} is not a merge"
            on_base = run_real_gh(["api", f"repos/{repo}/compare/{parents[1]}...{base}"])
            if not isinstance(on_base, dict) or on_base.get("status") not in ("ahead", "identical"):
                return f"merged parent {parents[1][:12]} is not on {base}"
            sha = parents[0]
        before_compare = run_real_gh(["api", f"repos/{repo}/compare/{base}...{approved}"])
        after_compare = run_real_gh(["api", f"repos/{repo}/compare/{base}...{head}"])
        if not isinstance(before_compare, dict) or not isinstance(after_compare, dict):
            return "the pull request's files are unreadable"
        before = _pr_files(before_compare, f"compare/{base}...{approved[:12]}")
        after = _pr_files(after_compare, f"compare/{base}...{head[:12]}")
        binary = sorted(name for name, (_, _, patch) in before.items() if not patch)
        if binary:
            return f"the pull request changes a binary file ({binary[0]})"
        if sorted(before) != sorted(after):
            changed = sorted(set(before) ^ set(after))
            return f"the set of files the pull request changes moved ({changed[0]})"
        for name, (status, sha, _) in sorted(before.items()):
            if after[name][:2] != (status, sha):
                return f"{name} at {head[:12]} is not the blob approved at {approved[:12]}"
        base_before = (before_compare.get("merge_base_commit") or {}).get("sha")
        base_after = (after_compare.get("merge_base_commit") or {}).get("sha")
        if not isinstance(base_before, str) or not isinstance(base_after, str):
            return "the merge bases are unreadable"
        if base_before != base_after:
            moved = run_real_gh(["api", f"repos/{repo}/compare/{base_before}...{base_after}"])
            if not isinstance(moved, dict):
                return "what the base changed is unreadable"
            both = sorted(set(before) & set(_pr_files(moved, "the base's changes")))
            if both:
                return f"{both[0]} was changed by both the pull request and the base"
    except GuardError as error:
        return f"the carry cannot be checked ({error})"
    return None


def _rest_pages(path: str) -> list[dict[str, Any]]:
    value = run_real_gh(["api", "--paginate", "--slurp", path])
    pages = value if isinstance(value, list) else [value]
    records: list[dict[str, Any]] = []
    for page in pages:
        if not isinstance(page, list):
            raise GuardError(f"{path} did not answer a list")
        records.extend(item for item in page if isinstance(item, dict))
    return records


def base_config_text(repo: str, base: str) -> str | None:
    """The base branch's ``.shipyard/config.toml``, or ``None`` when it has none."""
    real_gh = os.environ.get("GHAPP_REAL_GH", "/opt/homebrew/bin/gh")
    try:
        completed = subprocess.run(
            [real_gh, "api", "-H", "Accept: application/vnd.github.raw+json",
             f"repos/{repo}/contents/.shipyard/config.toml?ref={base}"],
            check=False, capture_output=True, text=True, timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise GuardError(f"cannot read the base config: {error}") from error
    if completed.returncode != 0:
        if "404" in completed.stderr or "Not Found" in completed.stderr:
            return None
        raise GuardError(f"cannot read the base config: {completed.stderr.strip()[-200:]}")
    return completed.stdout


def _binding_refusal(target: dict[str, Any], head: str) -> str | None:
    bound = target.get("bound_head")
    hint = (
        "`--match-head-commit <sha>` on `gh pr merge`, `expectedHeadOid` in the mutation, "
        "or `sha=` on the REST merge"
    )
    if not isinstance(bound, str) or not bound:
        return f"the request does not bind the head it lands; bind it to {head} with {hint}"
    if bound.lower() != head.lower():
        return f"the request binds {bound[:12]}, but the head is {head[:12]}"
    return None


def review_gate(classification: dict[str, Any], repo: str | None) -> tuple[bool, str]:
    """``(allowed, message)``: the review-before-arm rule for one target."""
    repo = classification.get("repo") or repo
    number = classification.get("pr")
    head = classification.get("head")
    label = f"PR #{number}"
    if not repo or not isinstance(number, int) or not isinstance(head, str):
        return False, f"{label}: cannot tell which head would land, so its review cannot be checked"
    if not host_review_default():
        # Off is inert: nothing is read, so no config, Python version or
        # GitHub answer can change what an arm does on a host that has not
        # turned review on.
        return True, f"review before arm is off on this host ({HOST_ENABLE_ENV} unset, no {HOST_ENABLE_FILE})"
    try:
        pull = run_real_gh(["api", f"repos/{repo}/pulls/{number}"])
        base = ((pull.get("base") or {}).get("ref")) if isinstance(pull, dict) else None
        if not isinstance(base, str) or not base:
            raise GuardError("the pull request carries no base branch")
        policy = review_policy(base_config_text(repo, base), repo, True)
        if not policy["required"]:
            return True, f"{repo} does not require a review before arming"
        live_head = ((pull.get("head") or {}).get("sha") or "").lower()
        if live_head != head.lower():
            return False, (
                f"{label}: the head moved while the guard read it ({head[:12]} -> "
                f"{live_head[:12]}); read it again"
            )
        author = (pull.get("user") or {}).get("login")
        author = author if isinstance(author, str) else None
        comments = _rest_pages(f"repos/{repo}/issues/{number}/comments?per_page=100")
        reviews = _rest_pages(f"repos/{repo}/pulls/{number}/reviews?per_page=100")
    except GuardError as error:
        return False, f"{label}: cannot read what its review requires ({error}); refusing to land it unreviewed"
    unbound = _binding_refusal(classification, head)
    if unbound is not None:
        return False, f"{label} ({repo}): {unbound}"
    allowed, why = review_verdict(head, author, comments, reviews, policy)
    if not allowed:
        approved = approved_earlier_head(head, author, comments, reviews, policy)
        if approved is not None:
            refusal = merge_only_carry(repo, base, approved, head)
            if refusal is None:
                return True, (
                    f"{label} ({repo}): the approval of {approved[:12]} carries to {head[:12]}: "
                    "every commit since is a merge of the base and the own lines are byte-identical"
                )
            why += f" The approval of {approved[:12]} does not carry: {refusal}."
    return allowed, f"{label} ({repo}): {why}"


def audit_override(message: str) -> None:
    """Append one line to the guards' override audit log. Never raises."""
    directory = _guards_dir()
    record = {
        "at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "override": REVIEW_OVERRIDE_ENV,
        "refusal": message,
        "argv": sys.argv[1:],
        "cwd": os.getcwd(),
    }
    try:
        directory.mkdir(parents=True, exist_ok=True)
        with (directory / "overrides.jsonl").open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(record, sort_keys=True) + "\n")
    except OSError:
        pass


# ---------------------------------------------------------------------------
# Live reads through the real gh binary and the App token ghapp exported.
# ---------------------------------------------------------------------------


def run_real_gh(arguments: list[str]) -> Any:
    real_gh = os.environ.get("GHAPP_REAL_GH", "/opt/homebrew/bin/gh")
    try:
        completed = subprocess.run(
            [real_gh, *arguments],
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise GuardError(f"cannot read live PR state: {error}") from error
    if completed.returncode != 0:
        detail = completed.stderr.strip().splitlines()
        raise GuardError(
            "cannot read live PR state: " + (detail[-1] if detail else f"exit {completed.returncode}")
        )
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise GuardError(f"live PR state was not JSON: {error}") from error


def graphql(query: str, variables: dict[str, str]) -> Any:
    arguments = ["api", "graphql", "-f", f"query={query}"]
    for key, value in variables.items():
        arguments += ["-F" if key == "number" else "-f", f"{key}={value}"]
    return run_real_gh(arguments)


def split_repo(repo: str) -> tuple[str, str]:
    parts = repo.strip().split("/")
    if len(parts) >= 2 and parts[-2] and parts[-1]:
        return parts[-2], parts[-1]
    raise GuardError(f"cannot resolve repository `{repo}`")


def base_has_merge_queue(owner: str, name: str, base: str) -> bool:
    response = graphql(BASE_QUEUE_QUERY, {"owner": owner, "name": name, "base": base})
    try:
        repository = response["data"]["repository"]
    except (KeyError, TypeError) as error:
        raise GuardError("cannot read whether the base branch has a merge queue") from error
    if response.get("errors") or not isinstance(repository, dict) or "mergeQueue" not in repository:
        raise GuardError("cannot read whether the base branch has a merge queue")
    return repository["mergeQueue"] is not None


# ---------------------------------------------------------------------------
# Batch attribution: who ejected this head, and does that failure implicate it?
#
# Two separable questions. Resolving the ejecting `merge_group` run and reading
# which jobs and steps failed is repo-agnostic and read-only. Deciding whether
# those failures implicate the head is repository knowledge: only the repository
# knows which of its steps compile or execute its own content. So the guard
# collects the evidence and asks the repository's declared attributor to rule on
# it. Absent a declaration nothing here runs.
# ---------------------------------------------------------------------------


def attributor_command(start: pathlib.Path | None = None) -> tuple[list[str], pathlib.Path] | None:
    """The repository's declared batch attributor as ``(argv, repo_root)``.

    Read from ``[queue.attribution] command`` in ``.shipyard/config.toml``,
    searched upward from ``start`` (the working directory ``ghapp`` was invoked
    in, the same provenance the rest of the guard uses). ``None`` when no
    repository declares one, which is the case that must read nothing.
    """
    try:
        import tomllib
    except ModuleNotFoundError:  # pragma: no cover - Python < 3.11
        return None
    here = (start or pathlib.Path.cwd()).resolve()
    for directory in (here, *here.parents):
        config = directory / ".shipyard" / "config.toml"
        if not config.is_file():
            continue
        try:
            parsed = tomllib.loads(config.read_text(encoding="utf-8"))
        except (OSError, ValueError) as error:
            raise GuardError(f"cannot read {config}: {error}") from error
        section = parsed.get("queue")
        section = section.get("attribution") if isinstance(section, dict) else None
        if not isinstance(section, dict):
            return None
        command = section.get("command")
        # An argv list only. A shell string would make the repository's config a
        # shell-injection surface for a command the guard runs on an operator's
        # machine, so it is rejected rather than quoted.
        if not isinstance(command, list) or not command or not all(
            isinstance(part, str) and part for part in command
        ):
            raise GuardError(
                f"[queue.attribution] command in {config} must be a non-empty list of strings"
            )
        return list(command), directory
    return None


def failed_jobs(owner: str, name: str, run_id: int) -> list[dict[str, Any]]:
    """Every failing job of ``run_id`` with the names of its failing steps."""
    # Not --paginate: `gh api --paginate` concatenates one JSON object per page,
    # which is not a JSON document. One large page instead.
    response = run_real_gh(
        ["api", f"repos/{owner}/{name}/actions/runs/{run_id}/jobs?per_page=100"]
    )
    jobs = response.get("jobs") if isinstance(response, dict) else None
    if not isinstance(jobs, list):
        raise GuardError(f"cannot read the jobs of run {run_id}")
    failures = []
    for job in jobs:
        if not isinstance(job, dict) or job.get("conclusion") != "failure":
            continue
        steps = job.get("steps") if isinstance(job.get("steps"), list) else []
        failures.append(
            {
                "job": job.get("name"),
                "job_id": job.get("id"),
                "failed_steps": [
                    step.get("name")
                    for step in steps
                    if isinstance(step, dict) and step.get("conclusion") == "failure"
                ],
            }
        )
    if not failures:
        # A failed run with no failing job is not evidence of anything.
        raise GuardError(f"run {run_id} reports no failing job")
    return failures


def _contains_head(owner: str, name: str, head: str, run_head: str) -> bool:
    """Whether ``run_head``'s history contains ``head``.

    ``compare/base...head`` reports status relative to the BASE, so a batch head
    built on top of this pull request's head is ``ahead`` of it. ``behind`` and
    ``diverged`` both mean the batch did not contain this head.
    """
    response = run_real_gh(["api", f"repos/{owner}/{name}/compare/{head}...{run_head}"])
    status = response.get("status") if isinstance(response, dict) else None
    return status in ("ahead", "identical")


def resolve_ejecting_batch(
    owner: str, name: str, number: int, head: str, at: str
) -> dict[str, Any]:
    """The failed ``merge_group`` run that ejected ``number`` at ``at``.

    A batch's read-only queue branch is named after one of its entries only, so
    naming is a fast path and commit ancestry is the fallback for a pull request
    that was not the batch's namesake. The listing is bounded by creation time
    around ``at`` and a page cap, not by recency, because a busy queue buries
    the ejector under later failures within the hour. Anything unresolved
    raises: a wrong run would attribute the wrong failure.
    """
    try:
        removed = datetime.strptime(at, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)
    except ValueError as error:
        raise GuardError(f"cannot read the removal time {at!r}") from error

    def _iso(moment: datetime) -> str:
        return moment.strftime("%Y-%m-%dT%H:%M:%SZ")

    window = (
        f"{_iso(removed - MERGE_GROUP_RUN_WINDOW)}..{_iso(removed + MERGE_GROUP_RUN_SKEW)}"
    )
    runs: list[Any] = []
    exhausted = False
    for page in range(1, MERGE_GROUP_RUN_MAX_PAGES + 1):
        response = run_real_gh(
            [
                "api",
                f"repos/{owner}/{name}/actions/runs"
                f"?event=merge_group&status=failure&created={window}"
                f"&per_page={MERGE_GROUP_RUN_PAGE}&page={page}",
            ]
        )
        listed = response.get("workflow_runs") if isinstance(response, dict) else None
        if not isinstance(listed, list):
            raise GuardError("cannot list failed merge_group runs")
        runs.extend(listed)
        if len(listed) < MERGE_GROUP_RUN_PAGE:
            exhausted = True
            break
    candidates = [
        run
        for run in runs
        if isinstance(run, dict)
        and isinstance(run.get("created_at"), str)
        and run["created_at"] <= at
        and isinstance(run.get("head_sha"), str)
    ]
    candidates.sort(key=lambda run: run["created_at"], reverse=True)
    named = f"pr-{number}-"

    def _selected(run: dict[str, Any]) -> dict[str, Any]:
        run_id = run.get("id")
        if not isinstance(run_id, int):
            raise GuardError("a failed merge_group run carries no id")
        return {
            "run_id": run_id,
            "workflow": run.get("name"),
            "url": run.get("html_url"),
            "head_sha": run["head_sha"],
            "created_at": run["created_at"],
            "failures": failed_jobs(owner, name, run_id),
        }

    def _branch(run: dict[str, Any]) -> str:
        branch = run.get("head_branch")
        return branch if isinstance(branch, str) else ""

    for run in candidates:
        if named in _branch(run):
            return _selected(run)
    # A batch's read-only queue branch names one entry, so a pull request that
    # was not the namesake needs commit ancestry. Bounded: each probe is a call.
    for run in candidates[:MERGE_GROUP_ANCESTRY_PROBES]:
        if _contains_head(owner, name, head, run["head_sha"]):
            return _selected(run)
    scope = (
        f"created {window}"
        if exhausted
        else f"in the newest {len(runs)} created {window} (read cap reached)"
    )
    raise GuardError(
        f"no failed merge_group run {scope} before {at} contains head {head[:12]}; "
        "the ejecting batch cannot be identified"
    )


def _describe(batch: dict[str, Any]) -> str:
    parts = []
    for failure in batch["failures"]:
        steps = ", ".join(step for step in failure["failed_steps"] if step) or "no failing step"
        parts.append(f"{failure['job']} ({steps})")
    return f"Batch run {batch['run_id']} failed: " + "; ".join(parts) + "."


def _stderr_tail(stderr: str | None) -> str:
    """The last few lines of an attributor's stderr, safe to print in a refusal.

    Control characters (terminal escape sequences included) are dropped, since
    the text is repository-produced and reaches an operator's terminal.
    """
    if not stderr:
        return ""
    printable = "".join(ch for ch in stderr if ch == "\n" or ch == "\t" or ch.isprintable())
    lines = [line.rstrip() for line in printable.splitlines() if line.strip()]
    tail = " | ".join(lines[-ATTRIBUTOR_STDERR_TAIL_LINES:])
    if len(tail) > ATTRIBUTOR_STDERR_TAIL_CHARS:
        tail = "..." + tail[-ATTRIBUTOR_STDERR_TAIL_CHARS:]
    return tail


def _with_stderr(detail: str, stderr: str | None) -> str:
    tail = _stderr_tail(stderr)
    return f"{detail} Attributor stderr (tail): {tail}" if tail else detail


def read_attributor_verdict(
    command: list[str], root: pathlib.Path, repo: str, number: int, batch: dict[str, Any]
) -> dict[str, Any]:
    """Run the attributor and return ``{"certified": bool, "detail": str}``.

    Certification requires all of: exit 0, a JSON object on stdout, a ``run_id``
    equal to the run the guard resolved, ``implicates_head`` exactly ``False``,
    and a ``verdict`` that positively names why. A verdict recording what was
    *not* found does not certify; neither does a missing or null
    ``implicates_head``, nor an ``other_pull_request`` verdict that names this
    same pull request.
    """
    argv = [*command, "--repo", repo, "--pr", str(number), "--run-id", str(batch["run_id"])]
    try:
        completed = subprocess.run(
            argv,
            check=False,
            capture_output=True,
            text=True,
            cwd=str(root),
            timeout=ATTRIBUTOR_TIMEOUT_SECONDS,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return {
            "certified": False,
            "detail": f"{_describe(batch)} The attributor did not run: {error}.",
        }
    evidence = _describe(batch)
    if completed.returncode != 0:
        return {
            "certified": False,
            "detail": _with_stderr(
                f"{evidence} The attributor exited {completed.returncode}, "
                "so it did not rule.",
                completed.stderr,
            ),
        }
    try:
        verdict = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        return {
            "certified": False,
            "detail": _with_stderr(
                f"{evidence} The attributor's verdict was not JSON: {error}.",
                completed.stderr,
            ),
        }
    if not isinstance(verdict, dict):
        return {
            "certified": False,
            "detail": f"{evidence} The attributor's verdict was not an object.",
        }
    if verdict.get("run_id") != batch["run_id"]:
        return {
            "certified": False,
            "detail": (
                f"{evidence} The attributor ruled on run {verdict.get('run_id')!r}, not "
                f"{batch['run_id']}, so its verdict does not apply."
            ),
        }
    label = verdict.get("verdict")
    if verdict.get("implicates_head") is not False or label not in CERTIFYING_VERDICTS:
        return {
            "certified": False,
            "detail": (
                f"{evidence} The attributor did not certify this head "
                f"(implicates_head={verdict.get('implicates_head')!r}, verdict={label!r})."
            ),
        }
    if label == "other_pull_request":
        other = verdict.get("implicated_pr")
        if not isinstance(other, int) or other == number:
            return {
                "certified": False,
                "detail": (
                    f"{evidence} The attributor blamed another pull request but named "
                    f"{other!r}, so nothing was attributed elsewhere."
                ),
            }
        return {
            "certified": True,
            "detail": f"{evidence} It attributed the failure to PR #{other}, not #{number}.",
        }
    return {
        "certified": True,
        "detail": f"{evidence} It attributed the failure to infrastructure: "
        + str(verdict.get("evidence") or "no evidence given"),
    }


def attribute_ejecting_batch(
    classification: dict[str, Any], repo: str | None
) -> dict[str, Any] | None:
    """Ask the repository whether the ejecting batch implicates this head.

    ``None`` whenever nothing can be asked -- no attributor declared, or the
    classification is not a same-head ejection for an attributable reason -- so
    the refusal stands untouched. A failure to resolve or to certify returns an
    uncertified verdict carrying the evidence, so the refusal can say why.
    """
    if classification.get("class") != "ejected":
        return None
    if classification.get("new_head_since_removal"):
        return None
    reason = str(classification.get("reason") or "").lower()
    if reason not in BATCH_ATTRIBUTABLE_REASONS:
        return None
    declared = attributor_command()
    if declared is None:
        return None
    command, root = declared
    # The attributor is declared by whichever checkout ghapp was invoked in. If
    # that is not the pull request's own repository, its ruling is about some
    # other repository's runs and must not certify anything here.
    try:
        local: str | None = "/".join(_require_parser().current_repo_identity())
    except Exception:  # noqa: BLE001 - any failure means "identity unknown"
        local = None
    number = classification.get("pr")
    head = classification.get("head")
    at = classification.get("at")
    repo = classification.get("repo") or repo
    if (
        not isinstance(number, int)
        or not isinstance(head, str)
        or not head
        or not isinstance(at, str)
    ):
        return {
            "certified": False,
            "detail": "The ejecting batch cannot be resolved from this state.",
        }
    if not isinstance(repo, str) or not repo:
        return {"certified": False, "detail": "The repository could not be resolved."}
    if local is not None and local.lower() != repo.lower():
        return {
            "certified": False,
            "detail": (
                f"The batch attributor declared in {root} belongs to {local}, not {repo}, "
                "so it cannot rule on this pull request."
            ),
        }
    owner, name = split_repo(repo)
    try:
        batch = resolve_ejecting_batch(owner, name, number, head, at)
    except GuardError as error:
        return {"certified": False, "detail": f"{error}."}
    return read_attributor_verdict(command, root, repo, number, batch)


# ---------------------------------------------------------------------------
# Environment ejections: one same-head re-enqueue after the network, not the
# head, failed the batch. The Python twin of src/environment_requeue.rs; both
# assert against tests/fixtures/github/job_logs/.
#
# Positive evidence only. Every failing REQUIRED check on the ejecting
# merge-group commit must be an Actions job whose every failing step (jobs API
# conclusion == failure) printed an environment signature within
# FAILURE_PROXIMITY_LINES output lines of that step's first ##[error]. A step's
# own `##[group]Run` script echo is never read as output: a script can quote
# the signature. Bounded to the head's first failed_checks/merge_conflict
# ejection, and off unless the repository sets
# `[queue.environment_requeue] enabled = true`.
# ---------------------------------------------------------------------------

NETWORK_TRANSPORT_MARKERS = (
    "Could not resolve host",
    "Network is unreachable",
    "No route to host",
    "Connection reset by peer",
)
ENVIRONMENT_SIGNATURES = NETWORK_TRANSPORT_MARKERS + (
    "ENOTFOUND",
    "getaddrinfo",
    "EAI_AGAIN",
    "Tunnel connection failed",
    "Proxy CONNECT aborted",
    "Temporary failure in name resolution",
    "ECONNRESET",
    "curl: (6)",
    "curl: (56)",
)
FAILURE_PROXIMITY_LINES = 60
# Twins of INTERRUPTION_SIGNATURES, STARVATION_MIN_WAIT_MINUTES and
# INTERRUPTION_REARM_LIMIT in src/environment_requeue.rs.
INTERRUPTION_SIGNATURES = ("Upload progress stalled",)
STARVATION_MIN_WAIT_MINUTES = 10
INTERRUPTION_REARM_LIMIT = 2
SUPERSEDED_ANNOTATIONS = ("higher priority waiting request",)
_RUN_GROUP = "##[group]Run "
_END_GROUP = "##[endgroup]"
_ERROR = "##[error]"
_STAMP = re.compile(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d")


def _split_timestamp(raw: str) -> tuple[str | None, str]:
    raw = raw.lstrip("\ufeff")
    if len(raw) < 20 or not _STAMP.match(raw):
        return None, raw
    end = raw.find("Z")
    if end < 0:
        return None, raw
    text = raw[end + 1 :]
    return raw[:19], text[1:] if text.startswith(" ") else text


def _segments(log: str) -> list[dict[str, Any]]:
    """Each ``##[group]Run`` header with the output after its ``##[endgroup]``."""
    segments: list[dict[str, Any]] = []
    in_echo = False
    for raw in log.splitlines():
        stamp, text = _split_timestamp(raw)
        if text.startswith(_RUN_GROUP):
            segments.append({"started": stamp, "output": [], "has_error": False})
            in_echo = True
            continue
        if not segments:
            continue
        if in_echo:
            if text.startswith(_END_GROUP):
                in_echo = False
            continue
        if text.startswith(_ERROR):
            segments[-1]["has_error"] = True
        segments[-1]["output"].append(text)
    return segments


def failing_step_output(log: str, started_at: str) -> list[str] | None:
    """Output of the step that started at ``started_at``, through its first ``##[error]``."""
    started_at = started_at[:19]
    if len(started_at) < 19:
        return None
    output: list[str] = []
    for segment in _segments(log):
        if segment["started"] is None or segment["started"] < started_at:
            continue
        if segment["has_error"]:
            lines = segment["output"]
            error = next(i for i, line in enumerate(lines) if line.startswith(_ERROR))
            output.extend(lines[: error + 1])
            return output
        output.extend(segment["output"])
    return None


def signature_near_failure(output: list[str]) -> dict[str, str] | None:
    for line in reversed(output[-FAILURE_PROXIMITY_LINES:]):
        for signature in ENVIRONMENT_SIGNATURES:
            if signature in line:
                return {"signature": signature, "line": line.strip()[:240]}
    return None


def interruption_at_failure(output: list[str]) -> dict[str, str] | None:
    """An interruption line on the step's ``##[error]`` line or the one before it."""
    for line in reversed(output[-2:]):
        for signature in INTERRUPTION_SIGNATURES:
            if signature in line:
                return {"signature": signature, "line": line.strip()[:240]}
    return None


def read_failing_step(log: str, started_at: str) -> dict[str, Any]:
    output = failing_step_output(log, started_at)
    if output is None:
        return {"reading": "not_located"}
    hit = signature_near_failure(output)
    if hit is not None:
        return {"reading": "environment", **hit}
    interrupted = interruption_at_failure(output)
    if interrupted is not None:
        return {"reading": "interruption", **interrupted}
    return {"reading": "no_signature"}


def starved_job(job: Any, annotations: Any = ()) -> dict[str, str] | None:
    """A required job cancelled with no runner after at least the minimum wait,
    and not superseded by a concurrency group."""
    if not isinstance(job, dict) or job.get("conclusion") != "cancelled":
        return None
    for annotation in annotations if isinstance(annotations, list) else []:
        message = annotation.get("message") if isinstance(annotation, dict) else None
        if isinstance(message, str) and any(text in message for text in SUPERSEDED_ANNOTATIONS):
            return None
    if str(job.get("runner_name") or "").strip():
        return None
    created, completed = _timestamp(job.get("created_at")), _timestamp(job.get("completed_at"))
    if created is None or completed is None:
        return None
    waited = int((completed - created).total_seconds() // 60)
    if waited < STARVATION_MIN_WAIT_MINUTES:
        return None
    return {"signature": "starved", "line": f"cancelled with no runner after {waited} min queued"}


def environment_requeue_enabled(start: pathlib.Path | None = None) -> bool:
    """Whether the nearest ``.shipyard/config.toml`` opts in."""
    try:
        import tomllib
    except ModuleNotFoundError:  # pragma: no cover - Python < 3.11
        return False
    here = (start or pathlib.Path.cwd()).resolve()
    for directory in (here, *here.parents):
        config = directory / ".shipyard" / "config.toml"
        if not config.is_file():
            continue
        try:
            parsed = tomllib.loads(config.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return False
        section = parsed.get("queue")
        section = section.get("environment_requeue") if isinstance(section, dict) else None
        return isinstance(section, dict) and section.get("enabled") is True
    return False


def run_real_gh_text(arguments: list[str]) -> str:
    real_gh = os.environ.get("GHAPP_REAL_GH", "/opt/homebrew/bin/gh")
    try:
        completed = subprocess.run(
            [real_gh, *arguments], check=False, capture_output=True, text=True, timeout=60
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise GuardError(str(error)) from error
    if completed.returncode != 0:
        detail = completed.stderr.strip().splitlines()
        raise GuardError(detail[-1] if detail else f"exit {completed.returncode}")
    return completed.stdout


def _describe_step(step: dict[str, Any]) -> str:
    reading = step["reading"]
    if reading == "environment":
        what = f"environment ({step['signature']}): {step['line']}"
    elif reading == "interruption":
        what = f"interruption ({step['signature']}): {step['line']}"
    elif reading == "no_signature":
        what = f"no environment signature within {FAILURE_PROXIMITY_LINES} lines of its failure"
    else:
        what = "its output could not be located in the job log"
    return f"{step['check']} / {step['step']} [job {step['job_id']}]: {what}"


def _required_contexts(api_json: Any, repo: str, base: str) -> list[str]:
    contexts: list[str] = []
    rules = api_json(["api", f"repos/{repo}/rules/branches/{base}"])
    for rule in rules if isinstance(rules, list) else []:
        if not isinstance(rule, dict) or rule.get("type") != "required_status_checks":
            continue
        parameters = rule.get("parameters") if isinstance(rule.get("parameters"), dict) else {}
        for check in parameters.get("required_status_checks") or []:
            if isinstance(check, dict) and isinstance(check.get("context"), str):
                contexts.append(check["context"])
    try:
        classic = api_json(["api", f"repos/{repo}/branches/{base}/protection/required_status_checks"])
    except GuardError as error:
        if "404" not in str(error):
            raise
        classic = {}
    for context in (classic.get("contexts") if isinstance(classic, dict) else None) or []:
        if isinstance(context, str):
            contexts.append(context)
    return sorted(set(contexts))


def assess_environment_requeue(
    classification: dict[str, Any],
    repo: str | None,
    opted_in: bool,
    api_json: Any = run_real_gh,
    api_text: Any = run_real_gh_text,
) -> dict[str, Any] | None:
    """Whether the one environment re-enqueue is allowed; ``None`` when it does not apply."""
    if classification.get("class") != "ejected" or classification.get("new_head_since_removal"):
        return None
    if str(classification.get("reason") or "").lower() != "failed_checks":
        return None
    ejection = classification.get("last_ejection") or {}
    sha = ejection.get("merge_group_commit")

    def refuse(reason: str, evidence: list[dict[str, Any]] | None = None) -> dict[str, Any]:
        return {"allowed": False, "reason": reason, "merge_group_commit": sha,
                "evidence": evidence or []}

    if not opted_in:
        return refuse(
            "this repository has not opted in to environment re-enqueues "
            "(`[queue.environment_requeue] enabled = true` in .shipyard/config.toml)"
        )
    if classification.get("timeline_complete") is not True:
        return refuse(
            "the timeline window does not reach the start of history, so an earlier ejection "
            "of this head cannot be ruled out"
        )
    count = classification.get("ejections_of_current_head")
    if not isinstance(count, int) or count < 1 or count > INTERRUPTION_REARM_LIMIT:
        return refuse(
            f"the queue has ejected this head {count} times; the re-enqueues a head is allowed "
            "have been spent"
        )
    if not isinstance(sha, str) or not sha:
        return refuse(
            "the removal names no merge-group commit, so the checks that ejected it cannot be read"
        )
    repo = classification.get("repo") or repo
    number = classification.get("pr")
    if not isinstance(repo, str) or not repo or not isinstance(number, int):
        return refuse("the repository or pull request could not be resolved")
    short = sha[:12]
    try:
        pull = api_json(["api", f"repos/{repo}/pulls/{number}"])
        base = (pull.get("base") or {}).get("ref") if isinstance(pull, dict) else None
        if not isinstance(base, str):
            return refuse(f"PR #{number} carries no base ref")
        try:
            required = _required_contexts(api_json, repo, base)
        except GuardError as error:
            return refuse(f"the required checks of `{base}` could not be read ({error})")
        if not required:
            return refuse(f"`{base}` declares no required checks, so no failure can be scoped to them")
        runs = api_json(["api", f"repos/{repo}/commits/{sha}/check-runs?per_page=100"])
        listed = runs.get("check_runs") if isinstance(runs, dict) else None
        if not isinstance(listed, list):
            return refuse(f"check runs of {short} could not be read")
        if runs.get("total_count") != len(listed):
            return refuse(f"{sha} has more check runs than one page returned; refusing a partial reading")
        failed: list[dict[str, Any]] = []
        for context in required:
            named = [run for run in listed if isinstance(run, dict) and run.get("name") == context]
            if not named:
                continue
            latest = max(named, key=lambda run: run.get("id") if isinstance(run.get("id"), int) else 0)
            if latest.get("status") != "completed":
                return refuse(f"required check `{context}` on {sha} has not completed")
            conclusion = latest.get("conclusion") or "none"
            if conclusion in ("success", "neutral", "skipped"):
                continue
            app = latest.get("app") if isinstance(latest.get("app"), dict) else {}
            failed.append({"name": context, "id": latest.get("id"), "conclusion": conclusion,
                           "actions_job": app.get("slug") == "github-actions"})
        statuses = api_json(["api", f"repos/{repo}/commits/{sha}/status"])
        for status in (statuses.get("statuses") if isinstance(statuses, dict) else None) or []:
            if (isinstance(status, dict) and status.get("context") in required
                    and status.get("state") in ("failure", "error")):
                failed.append({"name": status["context"], "id": None,
                               "conclusion": status["state"], "actions_job": False})
        if not failed:
            return refuse(
                f"no required check failed on merge-group commit {short}, so the ejection is "
                "unexplained"
            )
        evidence: list[dict[str, Any]] = []
        for check in failed:
            if not check["actions_job"] or not isinstance(check["id"], int):
                return refuse(
                    f"required check `{check['name']}` ({check['conclusion']}) is not a GitHub "
                    "Actions job, so it has no log to read"
                )
            job_id = check["id"]
            if check["conclusion"] == "cancelled":
                starved = starved_job(
                    api_json(["api", f"repos/{repo}/actions/jobs/{job_id}"]),
                    api_json(["api", f"repos/{repo}/check-runs/{job_id}/annotations"]),
                )
                evidence.append({"check": check["name"], "job_id": job_id, "step": "(queued)",
                                 **({"reading": "interruption", **starved} if starved
                                    else {"reading": "no_signature"})})
                continue
            if check["conclusion"] != "failure":
                return refuse(
                    f"required check `{check['name']}` concluded `{check['conclusion']}`, which is "
                    "not an environment signature"
                )
            job = api_json(["api", f"repos/{repo}/actions/jobs/{job_id}"])
            steps = job.get("steps") if isinstance(job, dict) else None
            failing = [step for step in steps or [] if isinstance(step, dict)
                       and step.get("conclusion") == "failure"]
            if not failing:
                return refuse(
                    f"`{check['name']}` [job {job_id}] failed with no failing step recorded, which "
                    "is not evidence of anything"
                )
            try:
                log = api_text(["api", f"repos/{repo}/actions/jobs/{job_id}/logs"])
            except GuardError as error:
                return refuse(
                    f"the log of `{check['name']}` [job {job_id}] could not be read ({error})"
                )
            for step in failing:
                started = step.get("started_at")
                reading = (read_failing_step(log, started) if isinstance(started, str)
                           else {"reading": "not_located"})
                evidence.append({"check": check["name"], "job_id": job_id,
                                 "step": step.get("name") or "", **reading})
    except GuardError as error:
        return refuse(f"{error}")
    unexplained = [_describe_step(step) for step in evidence
                   if step["reading"] not in ("environment", "interruption")]
    if unexplained:
        return refuse(
            f"a required failure on merge-group commit {short} is not an environment failure or "
            "an interruption: " + "; ".join(unexplained),
            evidence,
        )
    if any(step["reading"] == "interruption" for step in evidence):
        return {
            "allowed": True,
            "class": "interruption",
            "reason": (
                f"every failing required check on merge-group commit {short} was interrupted or "
                f"failed on the network, and this is ejection {count} of at most "
                f"{INTERRUPTION_REARM_LIMIT} for this head"
            ),
            "merge_group_commit": sha,
            "evidence": evidence,
        }
    if count != 1:
        return refuse(
            f"the queue has ejected this head {count} times; the one environment re-enqueue a "
            "head is allowed has been spent",
            evidence,
        )
    return {
        "allowed": True,
        "class": "network",
        "reason": (
            f"every failing required check on merge-group commit {short} failed on the network, "
            "and this is the head's first ejection"
        ),
        "merge_group_commit": sha,
        "evidence": evidence,
    }


# ---------------------------------------------------------------------------
# Request detection.
# ---------------------------------------------------------------------------

PR_MERGE_VALUE_OPTIONS = {
    "-A", "--author-email", "-b", "--body", "-F", "--body-file", "--match-head-commit",
    "-R", "--repo", "-t", "--subject",
}


def _require_parser() -> ModuleType:
    if PARSER is None:
        raise GuardError("request parser (queue-removal-guard) is unavailable")
    return PARSER


def pr_merge_target(args: list[str]) -> tuple[str | None, str | None]:
    """Return ``(selector, repo)`` for ``pr merge`` arguments."""
    selector = None
    skip = False
    for arg in args[2:]:
        if skip:
            skip = False
            continue
        if arg in PR_MERGE_VALUE_OPTIONS:
            skip = True
            continue
        if arg.startswith("-"):
            continue
        if selector is None:
            selector = arg
    repos = _require_parser().option_values(args[2:], {"-R", "--repo"})
    return selector, (repos[-1] if repos else None)


def resolve_merge_pr(args: list[str]) -> tuple[str, str, int]:
    selector, repo = pr_merge_target(args)
    if selector:
        match = re.search(r"github\.com/([^/]+)/([^/]+)/pull/([1-9][0-9]*)", selector)
        if match:
            return match.group(1), match.group(2), int(match.group(3))
    if repo is None:
        repo = os.environ.get("GH_REPO", "")
    if not repo:
        owner, name = _require_parser().current_repo_identity()
        repo = f"{owner}/{name}"
    owner, name = split_repo(repo)
    if selector and re.fullmatch(r"#?[1-9][0-9]*", selector):
        return owner, name, int(selector.lstrip("#"))
    view = ["pr", "view", "--repo", f"{owner}/{name}", "--json", "number"]
    if selector:
        view.insert(2, selector)
    value = run_real_gh(view)
    number = value.get("number") if isinstance(value, dict) else None
    if not isinstance(number, int):
        raise GuardError(f"cannot resolve the pull request for `{selector or 'current branch'}`")
    return owner, name, number


def graphql_variables(args: list[str]) -> dict[str, Any]:
    parser = _require_parser()
    variables: dict[str, Any] = {}
    for value in parser.option_values(args, {"-f", "-F", "--field", "--raw-field"}):
        key, separator, raw = value.partition("=")
        if not separator or key == "query":
            continue
        if raw == "@-":
            raise GuardError("cannot inspect a GraphQL variable read from stdin")
        if raw.startswith("@"):
            try:
                raw = pathlib.Path(raw[1:]).read_text(encoding="utf-8").strip()
            except OSError as error:
                raise GuardError(f"cannot inspect GraphQL variable file: {error}") from error
        variables[key] = raw
    for path in parser.option_values(args, {"--input"}):
        if path == "-":
            raise GuardError("cannot inspect a GraphQL body read from stdin")
        try:
            body = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise GuardError(f"cannot inspect GraphQL input: {error}") from error
        if isinstance(body, dict) and isinstance(body.get("variables"), dict):
            variables.update(body["variables"])
    return variables


_LANDING_MUTATION = re.compile(
    r"(enablePullRequestAutoMerge|enqueuePullRequest|mergePullRequest)\s*\(", re.IGNORECASE
)


def _argument(tail: str, name: str, variables: dict[str, Any]) -> str | None:
    """A mutation input field's value: a literal string or a resolved variable."""
    field = re.search(
        rf"\b{name}\s*:\s*(\$[A-Za-z_][A-Za-z0-9_]*|\"([^\"]+)\")", tail
    )
    if field is None:
        return None
    if field.group(2) is not None:
        return field.group(2)
    value = variables.get(field.group(1)[1:])
    if not isinstance(value, str) or not value:
        raise GuardError(f"cannot resolve mutation variable {field.group(1)}")
    return value


def landing_mutations(document: str, variables: dict[str, Any]) -> list[tuple[str, str, str | None]]:
    """``(action, pull request node id, bound head)`` for every arm, enqueue or merge."""
    found: list[tuple[str, str, str | None]] = []
    for match in _LANDING_MUTATION.finditer(document):
        tail = document[match.end():]
        close = tail.find(")")
        inputs = tail[: close if close >= 0 else len(tail)]
        target = _argument(inputs, "pullRequestId", variables)
        if target is None:
            raise GuardError(f"cannot find the pullRequestId {match.group(1)} targets")
        action = "merge" if match.group(1).lower() in MERGE_MUTATIONS else "arm"
        found.append((action, target, _argument(inputs, "expectedHeadOid", variables)))
    if not found:
        raise GuardError("landing mutation found but no pullRequestId could be read")
    return found


def arm_pull_request_ids(document: str, variables: dict[str, Any]) -> list[str]:
    """Every pull-request node id an arm mutation in ``document`` targets."""
    return [node for action, node, _ in landing_mutations(document, variables) if action == "arm"]


def rest_fields(args: list[str]) -> dict[str, Any]:
    """``-f``/``-F`` fields and a ``--input`` file's top-level object of a REST call."""
    fields: dict[str, Any] = {}
    for value in _require_parser().option_values(args, {"-f", "-F", "--field", "--raw-field"}):
        key, separator, raw = value.partition("=")
        if separator:
            fields[key] = raw
    for path in _require_parser().option_values(args, {"--input"}):
        if path == "-":
            raise GuardError("cannot inspect a REST body read from stdin")
        try:
            body = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise GuardError(f"cannot inspect REST input: {error}") from error
        if isinstance(body, dict):
            fields.update(body)
    return fields


def _target(classification: dict[str, Any], action: str, bound: Any) -> dict[str, Any]:
    return {**classification, "action": action,
            "bound_head": bound if isinstance(bound, str) else None}


def arm_request(args: list[str]) -> list[dict[str, Any]] | None:
    """``None`` when ``args`` can neither arm, enqueue nor merge; else the classified targets.

    Each target carries ``action`` (``arm`` or ``merge``) and the head the
    request binds (``bound_head``), when it binds one.
    """
    if "--help" in args or "-h" in args:
        return None
    if len(args) >= 2 and args[0] == "pr" and args[1] == "merge":
        if "--disable-auto" in args:
            return None
        auto = "--auto" in args
        owner, name, number = resolve_merge_pr(args)
        response = graphql(PR_BY_NUMBER_QUERY, {"owner": owner, "name": name, "number": str(number)})
        bound = _require_parser().option_values(args[2:], {"--match-head-commit"})
        action = "arm"
        if not auto:
            base = _base_ref(response)
            if base is None:
                raise GuardError("cannot read the pull request's base branch")
            # A direct merge: an administrator merge, or any merge on a base
            # with no queue. Either lands the head at once.
            if "--admin" in args or not base_has_merge_queue(owner, name, base):
                action = "merge"
        return [_target(classify_pr_queue_state(response), action, bound[-1] if bound else None)]
    if args and args[0] == "api":
        parser = _require_parser()
        try:
            endpoint, query = parser.api_target(args)
        except parser.GuardError as error:
            if any("graphql" in arg.lower() or arg.rstrip("/").endswith("/merge") for arg in args):
                raise GuardError(str(error)) from error
            return None
        if endpoint != "graphql":
            return rest_merge_request(args, endpoint)
        try:
            document = "\n".join([parser.graphql_document(args), *query.get("query", [])])
        except parser.GuardError as error:
            raise GuardError(str(error)) from error
        compact = "".join(document.split()).lower()
        if not any(name in compact for name in (*ARM_MUTATIONS, *MERGE_MUTATIONS)):
            return None
        return [
            _target(classify_pr_queue_state(graphql(PR_BY_ID_QUERY, {"id": node})), action, bound)
            for action, node, bound in landing_mutations(document, graphql_variables(args))
        ]
    return None


def rest_merge_request(args: list[str], endpoint: str) -> list[dict[str, Any]] | None:
    """The target of a REST ``PUT repos/<o>/<r>/pulls/<n>/merge``, else ``None``."""
    match = re.fullmatch(r"repos/([^/]+)/([^/]+)/pulls/([1-9][0-9]*)/merge", endpoint)
    if match is None:
        return None
    methods = _require_parser().option_values(args, {"-X", "--method"})
    fields = rest_fields(args)
    method = (methods[-1] if methods else ("POST" if fields else "GET")).upper()
    if method == "GET":
        return None
    owner, name, number = match.group(1), match.group(2), match.group(3)
    response = graphql(PR_BY_NUMBER_QUERY, {"owner": owner, "name": name, "number": number})
    return [_target(classify_pr_queue_state(response), "merge", fields.get("sha"))]


def _base_ref(response: Any) -> str | None:
    pr = _pull_request(response)
    base = pr.get("baseRefName") if pr else None
    return base if isinstance(base, str) and base else None


def _override(message: str) -> int:
    print(
        "queue-arm-guard: WARNING: GHAPP_ALLOW_QUEUE_REARM=1 overrides a refusal: " + message,
        file=sys.stderr,
    )
    return 0


def review_refusal(targets: list[dict[str, Any]], repo: str | None) -> bool:
    """Print and return whether an unreviewed head refuses the arm.

    GHAPP_ALLOW_UNREVIEWED_ARM=1 arms anyway, loudly and on the audit log.
    """
    unreviewed = [
        message
        for allowed, message in (review_gate(target, repo) for target in targets)
        if not allowed
    ]
    if not unreviewed:
        return False
    message = " | ".join(unreviewed)
    if os.environ.get(REVIEW_OVERRIDE_ENV) == "1":
        audit_override(message)
        print(
            f"queue-arm-guard: WARNING: {REVIEW_OVERRIDE_ENV}=1 lands an unreviewed head; "
            f"recorded in the guards' overrides.jsonl: {message}",
            file=sys.stderr,
        )
        return False
    print(
        f"queue-arm-guard: refusing to arm or merge an unreviewed head: {message} "
        "Post the verdict on the PR, then land it bound to that head.",
        file=sys.stderr,
    )
    return True


def main(args: list[str]) -> int:
    if os.environ.get("SHIPYARD_INTERNAL_QUEUE_MUTATION") == "1":
        return 0
    override = os.environ.get("GHAPP_ALLOW_QUEUE_REARM") == "1"
    try:
        targets = arm_request(args)
    except GuardError as error:
        message = f"refusing ambiguous queue-arm request: {error}"
        if override and os.environ.get(REVIEW_OVERRIDE_ENV) == "1":
            # Nothing names the head, so its review cannot be checked either.
            audit_override(message)
            return _override(message)
        if override:
            message += (
                f"; it names no head, so its review cannot be checked: "
                f"{REVIEW_OVERRIDE_ENV}=1 is needed as well"
            )
        print(
            f"queue-arm-guard: {message}. Inspect the PR with `shipyard landing --pr <n>` and "
            f"land it with `shipyard ship --pr <n>`. {OPERATOR_NOTE}",
            file=sys.stderr,
        )
        return 1
    if targets is None:
        return 0
    repo = os.environ.get("GH_REPO") or None
    verdicts = []
    # The queue-state rules judge arms and enqueues; a direct merge does not
    # enter the queue, so only the review rule applies to it.
    for target in (target for target in targets if target.get("action", "arm") == "arm"):
        try:
            attribution = attribute_ejecting_batch(target, repo)
        except GuardError as error:
            # A declared attributor the guard cannot even read is not a reason to
            # allow; it is a reason to say so and keep refusing.
            attribution = {"certified": False, "detail": f"{error}."}
        environment = None
        if not (attribution and attribution.get("certified") is True):
            try:
                environment = assess_environment_requeue(
                    target, repo, environment_requeue_enabled()
                )
            except GuardError as error:
                environment = {"allowed": False, "reason": f"{error}", "evidence": []}
        verdicts.append(decide(target, attribution, environment))
    refusals = [message for allowed, message in verdicts if not allowed]
    if refusals and not override:
        print(
            f"queue-arm-guard: refusing: {' | '.join(refusals)} Inspect it with "
            f"`shipyard landing --pr <n>`. {OPERATOR_NOTE}",
            file=sys.stderr,
        )
        return 1
    # The queue-state override does not stand in for a review.
    if review_refusal(targets, repo):
        return 1
    if refusals:
        return _override(" | ".join(refusals))
    for allowed, message in verdicts:
        if allowed and (
            "batch attributor certified" in message
            or "this re-enqueue is allowed" in message
        ):
            print(f"queue-arm-guard: note: {message}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

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


def arm_pull_request_ids(document: str, variables: dict[str, Any]) -> list[str]:
    """Every pull-request node id an arm mutation in ``document`` targets."""
    ids: list[str] = []
    mutation = re.compile(r"(enablePullRequestAutoMerge|enqueuePullRequest)\s*\(", re.IGNORECASE)
    for match in mutation.finditer(document):
        tail = document[match.end():]
        target = re.search(r"pullRequestId\s*:\s*(\$[A-Za-z_][A-Za-z0-9_]*|\"([^\"]+)\")", tail)
        if target is None:
            raise GuardError("cannot find the pullRequestId an arm mutation targets")
        if target.group(2) is not None:
            ids.append(target.group(2))
            continue
        name = target.group(1)[1:]
        value = variables.get(name)
        if not isinstance(value, str) or not value:
            raise GuardError(f"cannot resolve arm mutation variable ${name}")
        ids.append(value)
    if not ids:
        raise GuardError("arm mutation found but no pullRequestId could be read")
    return ids


def arm_request(args: list[str]) -> list[dict[str, Any]] | None:
    """``None`` when ``args`` cannot arm or enqueue; else the classified targets."""
    if "--help" in args or "-h" in args:
        return None
    if len(args) >= 2 and args[0] == "pr" and args[1] == "merge":
        if "--disable-auto" in args:
            return None
        auto = "--auto" in args
        if not auto and "--admin" in args:
            # A direct administrator merge bypasses the queue; it enqueues nothing.
            return None
        owner, name, number = resolve_merge_pr(args)
        response = graphql(PR_BY_NUMBER_QUERY, {"owner": owner, "name": name, "number": str(number)})
        if not auto:
            base = _base_ref(response)
            if base is None:
                raise GuardError("cannot read the pull request's base branch")
            if not base_has_merge_queue(owner, name, base):
                return None
        return [classify_pr_queue_state(response)]
    if args and args[0] == "api":
        parser = _require_parser()
        try:
            endpoint, query = parser.api_target(args)
            document = "\n".join([parser.graphql_document(args), *query.get("query", [])])
        except parser.GuardError as error:
            if any("graphql" in arg.lower() for arg in args):
                raise GuardError(str(error)) from error
            return None
        if endpoint != "graphql":
            return None
        compact = "".join(document.split()).lower()
        if not any(name in compact for name in ARM_MUTATIONS):
            return None
        ids = arm_pull_request_ids(document, graphql_variables(args))
        return [classify_pr_queue_state(graphql(PR_BY_ID_QUERY, {"id": node})) for node in ids]
    return None


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


def main(args: list[str]) -> int:
    if os.environ.get("SHIPYARD_INTERNAL_QUEUE_MUTATION") == "1":
        return 0
    override = os.environ.get("GHAPP_ALLOW_QUEUE_REARM") == "1"
    try:
        targets = arm_request(args)
    except GuardError as error:
        message = f"refusing ambiguous queue-arm request: {error}"
        if override:
            return _override(message)
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
    for target in targets:
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
    if not refusals:
        for allowed, message in verdicts:
            if allowed and (
                "batch attributor certified" in message
                or "this re-enqueue is allowed" in message
            ):
                print(f"queue-arm-guard: note: {message}", file=sys.stderr)
        return 0
    message = " | ".join(refusals)
    if override:
        return _override(message)
    print(
        f"queue-arm-guard: refusing: {message} Inspect it with `shipyard landing --pr <n>`. "
        f"{OPERATOR_NOTE}",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

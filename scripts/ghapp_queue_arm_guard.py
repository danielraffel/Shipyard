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
out of refusal text. Before each of those landings Shipyard runs this file
itself with ``SHIPYARD_QUEUE_ARM_REVIEW_ONLY=1`` (see ``review_only``): the
review rule below, unchanged, and nothing else, whether or not its ``gh`` is
the ``ghapp`` wrapper. Both that switch and the internal marker are honoured
only with the one-shot nonce Shipyard issues for that exact call (the removal
guard's ``consume_nonce``); without it they are ignored and the full rules
apply.

Every queue mutation is attributed on GitHub to the same App actor whether
Shipyard or an agent issued it, so the guard records each one it judges in
``queue-arms.jsonl`` (next to the removal guard's ``queue-removals.jsonl``):
time, host, session (``CLAUDE_CODE_SESSION_ID``, else
``CODEX_COMPANION_SESSION_ID``, else ``unattributed``), repository, pull
request, head, verb and verdict. ``shipyard who --pr N`` joins those records
to the pull request's timeline. A missing session never refuses anything.

Review before arm or merge
--------------------------

An arm, an enqueue or a direct merge (``pr merge`` on a queue-less base or with
``--admin``, REST ``PUT pulls/<n>/merge``, GraphQL ``mergePullRequest``) also
needs a reviewer verdict naming the exact head, in the repositories that
require one. The protected base's ``.shipyard/config.toml`` decides when it
sets ``[auto_merge] arm_requires_head_approval``. Otherwise the repositories in
REVIEW_REQUIRED_REPOS require one. It is on unless the host opted out with a
``review-before-arm.opt-out`` record in the guards directory, written with a
reason by ``shipyard guards install --opt-out-review-before-arm`` (anything
else at that path keeps it on; the opt-out is printed on every landing and
recorded in the arms ledger). With it
off the guard reads nothing for this rule, so it is inert whatever the config
or Python. With it on, a config that cannot be read or parsed refuses rather
than loosening.

An approval does not count for the agent that posted it: ``ghapp`` stamps
each verdict comment with ``<!-- session: <host>/<session> -->``, plus
``agent: <id>`` when ``SHIPYARD_AGENT_ID`` is set, and an arm whose agent id
equals the deciding approval's is refused. A session alone never refuses:
every agent of one Claude Code team shares ``CLAUDE_CODE_SESSION_ID``, so an
arm from the approving session only warns and is flagged in the arms ledger.
An unstamped verdict, or either side without an agent id, counts exactly as
before. This is the Python twin of
src/head_approval.rs; both read the policy from the base branch, never from
the head being armed.

A verdict is the last non-blank line of a pull-request comment,
``verdict: APPROVE head:<sha>`` or ``reviewed: <sha>`` (``verdict: HOLD
head:<sha>`` holds), or a review in state APPROVED / CHANGES_REQUESTED on the
head. A counting HOLD naming the head is final for it: no APPROVE naming the
same head, from any reviewer, before or after the HOLD, counts, and an
approval does not carry from a held head; only a new head (a push) can be
approved; each APPROVE ignored this way is reported as
``excluded_approval:head held by <signer> at <comment|review> <id>``.
Otherwise the latest verdict naming the head decides. When the base config
lists ``reviewer_identities``, only those count. Otherwise an approval counts
only from the fleet App (every agent posts as it through ghapp), or from an account the base lists in ``[auto_merge]
human_reviewers`` (empty by default) whose ``author_association`` is OWNER,
MEMBER or COLLABORATOR: any agent holding the owner's own gh login could post
a human approval. Each human approval excluded this way is printed and recorded
in the arms ledger (``excluded_approvals``). A HOLD from an OWNER, MEMBER or
COLLABORATOR still counts. Never other accounts (the repositories are public),
and never a distinguishable pull-request author.

The request must bind the head it lands: ``--match-head-commit <sha>``,
``expectedHeadOid``, or REST ``sha=``, equal to the head the verdict approved.

A merge-only update of an approved head carries the approval to the new head
only when every commit since is a two-parent merge whose merged parent is on
the base, and every file the pull request changes has the same blob at the new
head as at the approved head. A file the base also changed since, or any
binary change in the pull request, needs a new verdict.

A rebase of an approved head carries the approval only on a host that turned
the rebase carry on (``SHIPYARD_REBASE_CARRY=1`` or a ``rebase-carry`` file in
the guards directory), and only when all of these hold: the pull request's
diff has the same ``git patch-id --stable`` before and after; it changes the
same files with the same status; the base touched (changed, renamed or
deleted) none of those files between the two merge bases and every one keeps
its approved blob and mode, so re-applying the approved diff gives exactly the
new tree; and no required check on the new head has concluded ``skipped`` or
``neutral`` or reused a receipt. Every carry appends a line to
``carries.jsonl`` in the guards directory.

There is no override for this rule. ``GHAPP_ALLOW_UNREVIEWED_ARM=1`` is
refused: the guard says so and appends an audit line, and the refusal stands.
``GHAPP_ALLOW_QUEUE_REARM`` does not stand in for a review either.
"""

from __future__ import annotations

import importlib.machinery
import importlib.util
import json
import os
import pathlib
import posixpath
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
# `1` forces review on for one process (the review-verdict status uses it);
# nothing in the environment turns it off.
HOST_ENABLE_ENV = "SHIPYARD_REVIEW_BEFORE_ARM"
# The only way off: a file in the guards directory, holding the reason.
HOST_OPT_OUT_FILE = "review-before-arm.opt-out"
# The arms ledger, the mirror of the removal guard's queue-removals.jsonl.
ARMS_LOG_ENV = "SHIPYARD_QUEUE_ARMS_LOG"
SESSION_ENVS = ("CLAUDE_CODE_SESSION_ID", "CODEX_COMPANION_SESSION_ID")
# Per-agent identity the orchestrator sets; only it can refuse a self-approval.
AGENT_ENV = "SHIPYARD_AGENT_ID"
# `1`: Shipyard asks for the review rule alone before its own landing (see
# review_only). It judges queue state itself.
REVIEW_ONLY_ENV = "SHIPYARD_QUEUE_ARM_REVIEW_ONLY"
# Which Shipyard path is landing, for the arms ledger.
CALLER_ENV = "SHIPYARD_QUEUE_ARM_CALLER"
UNATTRIBUTED = "unattributed"
# `<!-- session: <host>/<session>[ agent: <id>] -->`, stamped by ghapp on
# verdict comments: exactly the ASCII form the stamper writes, single spaces
# and printable ASCII only. Same pattern as src/head_approval.rs stamp_pattern.
_SESSION_STAMP = re.compile(
    r"<!-- session: ([!-.0-~]+)/([!-~]+?)(?: agent: ([!-~]+?))? -->")
# Anything else that reads like a stamp: other spacing or case, or Unicode
# whitespace (Shipyard#881 review: `<!--` + U+00A0 + `session:` passed ghapp's
# byte check and counted here as an m3 stamp). An App APPROVE carrying one is
# refused, never read as stamped.
_STAMP_VARIANT = re.compile(r"<!--\s*session\s*:", re.IGNORECASE)


def noncanonical_stamp(body: str) -> bool:
    """Whether ``body`` has stamp-like text that is not an exact stamp."""
    return _STAMP_VARIANT.search(_SESSION_STAMP.sub("", body)) is not None
# The rebase carry (see rebase_carry) is a separate switch, off by default.
REBASE_CARRY_ENV = "SHIPYARD_REBASE_CARRY"
REBASE_CARRY_FILE = "rebase-carry"
REBASE_CARRY_LOG = "carries.jsonl"
# Paths whose change means an approval never carries to a new head: a carry
# must never be the way a workflow, the CI contract, the guards or Shipyard's
# own arm code, or signing and release automation lands unreviewed. The base's
# `[auto_merge] carry_excluded_paths` replaces the list,
# `carry_excluded_paths_extra` adds to it, and `carry_excluded_paths_allow`
# re-includes default paths outside PROTECTED_CARRY_PATHS (see carry_paths).
# Same as src/head_approval.rs DEFAULT_CARRY_EXCLUDED_PATHS.
DEFAULT_CARRY_EXCLUDED_PATHS = (
    ".github/workflows/**",
    ".agents/contract.toml",
    ".shipyard/**",
    "tools/ci/**",
    "scripts/ghapp*",
    "src/head_approval*",
    "src/head_approval/**",
    "src/queue_arm_gate*",
    "src/auto_arm*",
    "src/auto_arm/**",
    "src/ghapp_guards*",
    "src/app/ship_cmd/auto_arm*",
    "src/app/merge_steward_cmd/native_arm*",
    "release-please-config.json",
    ".release-please-manifest.json",
    "**/*codesign*",
    "**/*notariz*",
    "**/*signing*",
)
# Default exclusions no `carry_excluded_paths_allow` entry re-includes:
# workflows, the CI contract, the carry policy itself, the guards and
# Shipyard's arm code, and signing. A `**/` name glob is enforced on every match
# (see carry_excluded) rather than when the allow list is read. Same as
# src/head_approval.rs PROTECTED_CARRY_PATHS.
PROTECTED_CARRY_PATHS = (
    ".github/workflows/**",
    ".agents/contract.toml",
    ".shipyard/config.toml",
    "tools/ci/**",
    "scripts/ghapp*",
    "src/head_approval*",
    "src/head_approval/**",
    "src/queue_arm_gate*",
    "src/auto_arm*",
    "src/auto_arm/**",
    "src/ghapp_guards*",
    "src/app/ship_cmd/auto_arm*",
    "src/app/merge_steward_cmd/native_arm*",
    "**/*codesign*",
    "**/*notariz*",
    "**/*signing*",
)
# An approval carries across at most this many base merges, and for at most
# this long after its verdict; past either, the head needs a new verdict.
MAX_CARRY_MERGES = 3
MAX_CARRY_AGE = timedelta(hours=48)
# A line in an APPROVE that keeps its approval to the exact head named.
_NO_CARRY_LINE = re.compile(r"^[ \t]*carry[ \t]*:[ \t]*no[ \t]*$", re.MULTILINE | re.IGNORECASE)
# The guards directory's override audit log.
OVERRIDES_LOG = "overrides.jsonl"
# A required check that concluded one of these on the new head did not run in full.
NOT_RUN_IN_FULL = ("skipped", "neutral")
# The annotation a job prints when it decides whether a receipt is reused.
RECEIPT_DECISION_TITLE = "shipyard-receipt-decision"
# Tier records: check runs named TIER_RECORD_PREFIX + <check>, created by the
# App the base names in `[queue] tier_record_app`, saying which test tier a run
# attempt ran. Never the Actions app, whose records a pull request job could
# PATCH, nor the fleet App every agent posts as. Same as src/deferred_tier.rs.
TIER_RECORD_PREFIX = "shipyard-test-tier/"
TIER_RECORD_SCHEMA = "shipyard-test-tier/v1"
# Optional repository-side proof that a durable reviewer claim was consumed.
# Both the queue-arm guard and the review-verdict workflow call the parser
# below, so enabling this policy cannot make the two decisions diverge.
CLAIM_RECEIPT_KEY = "reviewer_claim_receipt"
CLAIM_APP_KEY = "reviewer_claim_app"
CLAIM_RECEIPT_SCHEMA = "shipyard-reviewer-claim/v1"
ACTIONS_APP = "github-actions"
# GitHub Actions' App id is shared by every workflow and cannot be used as a
# durable reviewer authority.  A claim policy naming it is refused at parse
# time rather than silently turning the optional gate into a shared token.
SHARED_CLAIM_APP_ID = 15368
DEFERRED_TIER = "deferred"
# The log group the runner prints in "Set up job" listing the token's scopes.
TOKEN_PERMISSIONS_HEADER = "GITHUB_TOKEN Permissions"
# The workflow_dispatch input naming the pull request whose merge ref
# (refs/pull/<n>/merge) the full lane checks out.
FULL_LANE_PR_INPUT = "pr_number"
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


def valid_opt_out() -> str | None:
    """The reason of this host's opt-out, or ``None`` when there is no valid one.

    Only a regular file holding the JSON ``shipyard guards install
    --opt-out-review-before-arm`` writes, with a non-empty ``reason``, ``at``
    and ``by``, counts. A directory, an empty or hand-made file, or garbage
    leaves review on (and ``guards status`` fails on it). The Rust twin is
    ``head_approval::read_opt_out``.
    """
    path = _guards_dir() / HOST_OPT_OUT_FILE
    try:
        if not path.is_file():
            return None
        record = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    if not isinstance(record, dict):
        return None
    fields = [record.get(key) for key in ("reason", "at", "by")]
    if not all(isinstance(value, str) and value.strip() for value in fields):
        return None
    return fields[0].strip()


def host_review_default() -> bool:
    """Whether this host applies REVIEW_REQUIRED_REPOS: on unless the guards
    directory holds a valid opt-out (see ``valid_opt_out``)."""
    if os.environ.get(HOST_ENABLE_ENV) == "1":
        return True
    return valid_opt_out() is None


def opt_out_reason() -> str:
    """The recorded reason of this host's opt-out, for messages and the ledger."""
    return valid_opt_out() or "no valid opt-out"


def host_name() -> str:
    import socket
    return socket.gethostname().split(".")[0] or "unknown-host"


def session_id() -> str:
    """This process's agent session, or ``unattributed``; never an error."""
    for name in SESSION_ENVS:
        value = os.environ.get(name, "").strip()
        if value:
            return value
    return UNATTRIBUTED


def agent_id() -> str | None:
    """This process's per-agent id (``SHIPYARD_AGENT_ID``), when set."""
    return os.environ.get(AGENT_ENV, "").strip() or None


def stamped_session(body: Any) -> str | None:
    """The session a verdict comment was stamped with, if any."""
    if not isinstance(body, str):
        return None
    found = _SESSION_STAMP.findall(body)
    return found[-1][1] if found else None


def stamped_agent(body: Any) -> str | None:
    """The agent id a verdict comment was stamped with, if any."""
    if not isinstance(body, str):
        return None
    found = _SESSION_STAMP.findall(body)
    return (found[-1][2] or None) if found else None


def stamped_identity(body: Any) -> str | None:
    """The provider-neutral ``host agent:id`` identity in a ghapp stamp."""
    if not isinstance(body, str):
        return None
    found = _SESSION_STAMP.findall(body)
    if not found or not found[-1][2]:
        return None
    host, _session, agent = found[-1]
    return f"{host} agent:{agent}".lower()


def writer_identity() -> dict[str, str]:
    """``session`` (and ``agent`` when set) for every row a guard writes, so a
    row says which process wrote it."""
    agent = agent_id()
    return {"session": session_id(), **({"agent": agent} if agent else {})}


def arms_log_path() -> pathlib.Path:
    configured = os.environ.get(ARMS_LOG_ENV)
    if configured:
        return pathlib.Path(configured)
    state = os.environ.get("XDG_STATE_HOME") or str(pathlib.Path.home() / ".local" / "state")
    return pathlib.Path(state) / "shipyard" / "queue-arms.jsonl"


def record_arms(targets: list[dict[str, Any]] | None, repo: str | None, verdict: str,
                review: str) -> None:
    """Append one ledger entry per judged target. Never raises: the ledger
    attributes landings, it does not gate them."""
    if not targets:
        return
    try:
        path = arms_log_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("a", encoding="utf-8") as log:
            for target in targets:
                log.write(json.dumps({
                    "at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
                    "host": host_name(),
                    "session": session_id(),
                    "repo": target.get("repo") or repo,
                    "pr": target.get("pr"),
                    "head": target.get("head"),
                    "verb": target.get("action", "arm"),
                    "verdict": verdict,
                    "review": review,
                    **({"agent": agent_id()} if agent_id() else {}),
                    **({"same_session_approval": True}
                       if target.get("same_session_approval") else {}),
                    **({"excluded_approvals": target["excluded_approvals"]}
                       if target.get("excluded_approvals") else {}),
                    **({"node": target["node"]} if target.get("node") else {}),
                    **_approval_fields(target.get("approval")),
                    **({"caller": os.environ[CALLER_ENV]} if os.environ.get(CALLER_ENV) else {}),
                }, sort_keys=True) + "\n")
    except OSError as error:
        print(f"queue-arm-guard: note: the arms ledger was not written: {error}", file=sys.stderr)


def _approval_fields(approval: Any) -> dict[str, Any]:
    """The deciding verdict's comment id and signer, for the arms ledger."""
    if not isinstance(approval, dict):
        return {}
    return {key: approval[key] for key in ("verdict_comment_id", "verdict_review_id", "signer",
                                           "approved_head")
            if approval.get(key) is not None}


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
    humans = section.get("human_reviewers") or []
    # The protected base may name its own signers; otherwise this host's
    # signers file decides (see reviewer_signers).
    from_base_only = os.environ.get(SIGNERS_FROM_BASE_ENV) == "1"
    configured = section.get("reviewer_signers")
    if configured is not None and not (
            isinstance(configured, list) and all(isinstance(name, str) for name in configured)):
        # Never fall back to the host's file on a malformed list.
        raise GuardError("the base's .shipyard/config.toml [auto_merge] reviewer_signers must "
                         f"be a list of reviewer names, not {configured!r}")
    configured_signers = ([name.lower() for name in configured]
                          if configured is not None else None)
    claim_receipt = section.get(CLAIM_RECEIPT_KEY)
    claim_app = section.get(CLAIM_APP_KEY)
    if claim_receipt is None and claim_app is None:
        claim_receipt = claim_app = None
    elif not (isinstance(claim_receipt, str) and claim_receipt.strip()):
        raise GuardError(f"the base's .shipyard/config.toml [auto_merge] {CLAIM_RECEIPT_KEY} "
                         "must be a non-empty check-run name when claim receipts are enabled")
    elif not ((isinstance(claim_app, str) and claim_app.strip())
              or (type(claim_app) is int and claim_app > 0)):
        raise GuardError(f"the base's .shipyard/config.toml [auto_merge] {CLAIM_APP_KEY} "
                         "must be a non-empty app slug or positive app id when claim receipts are enabled")
    if isinstance(claim_app, str) and claim_app.strip().lower() in {ACTIONS_APP, FLEET_AUTHOR}:
        raise GuardError(f"the base's .shipyard/config.toml [auto_merge] {CLAIM_APP_KEY} "
                         f"may not name the shared `{claim_app}` App")
    if type(claim_app) is int and claim_app == SHARED_CLAIM_APP_ID:
        raise GuardError(f"the base's .shipyard/config.toml [auto_merge] {CLAIM_APP_KEY} "
                         f"may not name shared GitHub Actions App id {SHARED_CLAIM_APP_ID}")
    identity_map = section.get("reviewer_signer_identities")
    if identity_map is not None:
        if not isinstance(identity_map, dict) or any(
                not isinstance(name, str) or not isinstance(values, list)
                or not values or not all(isinstance(value, str) and value.strip() for value in values)
                for name, values in identity_map.items()):
            raise GuardError("the base's .shipyard/config.toml [auto_merge] "
                             "reviewer_signer_identities must map reviewer names to non-empty "
                             "lists of host agent identities")
        identity_map = {
            name.lower(): [value.strip().lower() for value in values]
            for name, values in identity_map.items()
        }
    try:
        excluded, carry_error = carry_paths(section), None
    except GuardError as error:
        # A refused configuration: no carry at all, as src/head_approval.rs reads it.
        excluded, carry_error = ["**"], str(error)
    return {
        # Paths whose change keeps an approval from carrying (see carry_excluded).
        "carry_excluded": (excluded if excluded is not None
                           else list(DEFAULT_CARRY_EXCLUDED_PATHS)),
        # Why the base's carry configuration was refused, or None.
        "carry_error": carry_error,
        "required": required if isinstance(required, bool) else default,
        "reviewers": [str(login).lower() for login in reviewers if isinstance(login, str)],
        # Required by REVIEW_REQUIRED_REPOS, not by the repository's own key.
        "any_but_author": from_default,
        # Human accounts whose approvals count under any_but_author; empty by
        # default, so only the fleet App approves.
        "humans": [_normalized_login(login) for login in humans if isinstance(login, str)],
        # Who may sign the fleet App's APPROVE on this host (see
        # reviewer_signers); None when the host has no signers file.
        "signers": (configured_signers if configured_signers is not None or from_base_only
                    else reviewer_signers(repo)),
        "signer_identities": identity_map or {},
        # The `session:` lines this host accepts on an App APPROVE (see
        # reviewer_sessions); None when the host lists none. Host-local only, so
        # the repository-side rule never reads it.
        "sessions": None if from_base_only else reviewer_sessions(),
        # Which `ghapp` stamp hosts each short host name in a `session:` line
        # stands for (see reviewer_session_hosts). Host-local, like sessions.
        "session_hosts": {} if from_base_only else reviewer_session_hosts(),
        # The repository-side rule (review-verdict status): no host's file,
        # and a default-mode repository with no list admits no App APPROVE.
        "signers_required": from_base_only and from_default and not reviewers,
        # Optional and default-off: existing stamped reviewer rules remain the
        # authority until both protected-base fields are present.
        "claim_receipt": claim_receipt.strip() if isinstance(claim_receipt, str) else None,
        "claim_app": claim_app.strip() if isinstance(claim_app, str) else claim_app,
    }


# Guards-directory file naming who may sign the fleet App's APPROVE:
# {"default": ["pulp-review", ...], "repos": {"OWNER/REPO": [...]}}.
SIGNERS_FILE = "reviewer-signers.json"
# `1`: signers only from the base config (the server-side status pins it).
SIGNERS_FROM_BASE_ENV = "SHIPYARD_SIGNERS_FROM_BASE"
SIGNER_IDENTITIES_KEY = "reviewer_signer_identities"
_SIGNER_LINE = re.compile(r"^[ \t]*reviewer:[ \t]*([A-Za-z0-9._-]+)[ \t]*$", re.MULTILINE)


def reviewer_signers(repo: str) -> list[str] | None:
    """The reviewer names this host's signers file allows for an App APPROVE
    in ``repo``, or None when the host has no signers file. A base config's
    ``[auto_merge] reviewer_signers`` takes precedence (see review_policy). A file that cannot
    be read admits nobody: an unreadable allowlist never loosens the rule."""
    path = _guards_dir() / SIGNERS_FILE
    if not path.exists():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        repos = data.get("repos") or {}
        names = next((value for key, value in repos.items() if key.lower() == repo.lower()),
                     data.get("default"))
        if not isinstance(names, list):
            return []
        return [str(name).lower() for name in names if isinstance(name, str)]
    except (OSError, ValueError, AttributeError):
        return []


# The `sessions` key of SIGNERS_FILE: the exact `session:` lines a reviewer
# writes on its verdicts, e.g. ["session: pulp-review (team-lead team, ...)"].
SESSIONS_KEY = "sessions"
# A `session:` line a reviewer writes itself. The `<!-- session: ... -->`
# stamp ghapp adds is not one: it starts with `<!--`.
_SESSION_LINE = re.compile(r"^[ \t]*session:[ \t]*(.*?)[ \t]*$", re.MULTILINE)
_SESSIONS_WARNED = False


def _session_value(line: str) -> str:
    """``line`` without a leading ``session:``, so an allowlist entry may be
    written either way."""
    text = line.strip()
    return text[len("session:"):].strip() if text.lower().startswith("session:") else text


def reviewer_sessions() -> list[str] | None:
    """The ``session:`` lines this host's signers file accepts on an App
    APPROVE, or None when the file is absent or has no ``sessions`` key (the
    rule is off, with a warning). A file that cannot be read, or a ``sessions``
    value that is not a list of strings, admits nobody."""
    path = _guards_dir() / SIGNERS_FILE
    if not path.exists():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []
    if not isinstance(data, dict) or SESSIONS_KEY not in data:
        return None if isinstance(data, dict) else []
    lines = data[SESSIONS_KEY]
    if not isinstance(lines, list) or not all(isinstance(line, str) for line in lines):
        return []
    return [value for value in (_session_value(line) for line in lines) if value]


# The `session_hosts` key of SIGNERS_FILE: for each short host name a
# `session:` line names (`host m3`), the hostnames `ghapp` stamps from that
# machine, e.g. {"m3": ["Daniels-Mac-Studio-m3"]}. A hostname is not
# predictable from the short name (macOS appends `-2`, `-4` on a clash), so the
# map is written out rather than derived.
SESSION_HOSTS_KEY = "session_hosts"
# The host a `session:` line names: its last `host <name>`, e.g.
# `pulp-review (team-lead team, agent pulp-review, host m3)`.
_SESSION_HOST = re.compile(r"\bhost[ \t]+([^\s,()]+)[ \t]*\)?[ \t]*$")


def reviewer_session_hosts() -> dict[str, list[str]]:
    """This host's ``session_hosts`` map, short host name to the stamp
    hostnames it stands for, both lowercased. A missing file or key, or a value
    that is not an object of string lists, maps nothing, so with a session
    allowlist no App APPROVE is admitted."""
    path = _guards_dir() / SIGNERS_FILE
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    hosts = data.get(SESSION_HOSTS_KEY) if isinstance(data, dict) else None
    if not isinstance(hosts, dict) or not all(
            isinstance(names, list) and all(isinstance(name, str) for name in names)
            for names in hosts.values()):
        return {}
    return {str(short).strip().lower(): [name.strip().lower() for name in names if name.strip()]
            for short, names in hosts.items()}


def stamp_refusal(text: str, found: list[str], hosts: dict[str, list[str]]) -> str | None:
    """Why ``text``'s ``ghapp`` stamp does not match the host its ``session:``
    lines ``found`` name, or None. There must be exactly one stamp, and every
    line must name a host that ``hosts`` maps to the stamp's hostname: a
    copied ``session:`` line keeps the reviewer's host, while the stamp names
    the machine that actually posted (tartci#623)."""
    stamps = _SESSION_STAMP.findall(text)
    if not stamps:
        return "it has no `ghapp` session stamp"
    if len(stamps) > 1:
        return f"it carries {len(stamps)} `ghapp` session stamps"
    stamp_host = stamps[0][0].lower()
    for line in found:
        named = _SESSION_HOST.search(line)
        if named is None:
            return f"its session line `session: {line}` names no host"
        short = named.group(1).lower()
        if short not in hosts:
            return f"host {short} has no `{SESSION_HOSTS_KEY}` entry on this host"
        if stamp_host not in hosts[short]:
            return (f"its stamp host {stamps[0][0]} is not host {short} "
                    f"({', '.join(hosts[short]) or 'no hostnames'})")
    return None


def _warn_no_session_allowlist() -> None:
    global _SESSIONS_WARNED
    if not _SESSIONS_WARNED:
        _SESSIONS_WARNED = True
        print(f"queue-arm-guard: WARNING: no `{SESSIONS_KEY}` list in "
              f"{_guards_dir() / SIGNERS_FILE}; an App APPROVE is judged without its "
              "`session:` line, so a copied `reviewer:` line passes", file=sys.stderr)


def session_refusal(text: str, sessions: list[str] | None,
                    hosts: dict[str, list[str]] | None = None) -> str | None:
    """Why ``text``'s ``session:`` lines do not admit an App APPROVE, or None.
    Every such line must be listed, there must be at least one, and the
    ``ghapp`` stamp must come from the host they name (see stamp_refusal)."""
    if sessions is None:
        _warn_no_session_allowlist()
        return None
    found = [_session_value(line) for line in _SESSION_LINE.findall(text)]
    if not found:
        return "it has no `session:` line"
    unlisted = [line for line in found if line not in sessions]
    if unlisted:
        return f"its session line `session: {unlisted[0]}` is not on this host's allowlist"
    return stamp_refusal(text, found, reviewer_session_hosts() if hosts is None else hosts)


def app_approval_refusal(body: Any, policy: dict[str, Any]) -> str | None:
    """Why the fleet App's APPROVE in ``body`` does not count, or None.

    Every agent posts as the same App, so the App's identity says nothing
    about who reviewed. An APPROVE with stamp-like text that is not an exact
    stamp never counts, and one stamped by ``ghapp`` with an
    ``unattributed`` session never counts, nor, under the repository-side rule
    (``review-verdict``), one with no stamp at all; on a host with a session
    allowlist every ``session:`` line must be listed (and one must be present)
    and the single ``ghapp`` stamp must come from the host those lines name,
    and on a
    host with a signers file the body must carry a ``reviewer: <name>`` line
    naming an allowed signer.
    """
    text = body if isinstance(body, str) else ""
    if noncanonical_stamp(text):
        return "it carries a non-canonical session stamp"
    stamp = _SESSION_STAMP.findall(text)
    if stamp and stamp[-1][1] == UNATTRIBUTED:
        return "its session stamp is unattributed"
    if os.environ.get(SIGNERS_FROM_BASE_ENV) == "1" and not stamp:
        # The repository-side rule: an APPROVE posted around ghapp's stamper
        # (stdin, a raw API call) names no session at all (tartci#623's copy,
        # stamp removed, still passed this status).
        return "it has no `ghapp` session stamp"
    if os.environ.get(SIGNERS_FROM_BASE_ENV) != "1":
        # Host-local: the repository-side rule has no host to read.
        refusal = session_refusal(
            text, policy["sessions"] if "sessions" in policy else reviewer_sessions(),
            policy.get("session_hosts"))
        if refusal is not None:
            return refusal
    signers = policy.get("signers")
    if signers is None and policy.get("signers_required"):
        return ("no signer list is configured on the base branch "
                "(`[auto_merge] reviewer_signers`)")
    if signers is None:
        return None
    signed = [name.lower() for name in _SIGNER_LINE.findall(text)]
    if not signed:
        return "it has no `reviewer: <name>` line"
    matching = [name for name in signed if name in signers]
    if not matching:
        return f"its signer {signed[-1]} is not one of {', '.join(signers) or 'nobody'}"
    identities = policy.get("signer_identities") or {}
    if identities:
        stamp_identity = stamped_identity(text)
        if stamp_identity is None:
            return "its ghapp stamp has no reviewer agent identity"
        bound = next((name for name in matching
                      if name in identities and stamp_identity in identities[name]), None)
        if bound is None:
            name = matching[-1]
            if name not in identities:
                return f"its signer {name} has no base identity binding"
            return (f"its signer {name} is not bound to stamped identity "
                    f"{stamp_identity}")
    return None


# Human approvals the default rule refused this run, for the warning and the
# arms ledger (see review_gate).
EXCLUDED_APPROVALS: list[str] = []
# APPROVEs ignored this run because a HOLD names the same head, as
# ``excluded_approval:head held by <signer> at <comment|review> <id>`` (see
# review_verdict and review_gate).
HELD_EXCLUSIONS: list[str] = []


def _reviewer_counts(
    login: str, association: Any, policy: dict[str, Any], author: str | None,
    verdict: str, bot: bool = False, body: Any = None,
) -> bool:
    reviewer = _normalized_login(login)
    if reviewer == FLEET_AUTHOR and not bot:
        # Only the App itself is a Bot; a user account named like it is
        # anyone at all.
        EXCLUDED_APPROVALS.append(f"{login} (not the App: a user account)")
        return False
    if reviewer == FLEET_AUTHOR and verdict == "APPROVE":
        refusal = app_approval_refusal(body, policy)
        if refusal is not None:
            EXCLUDED_APPROVALS.append(f"{login} ({refusal})")
            return False
    if policy.get("any_but_author") and not policy["reviewers"]:
        # The repositories are public: only the fleet App and accounts the
        # repository trusts count. Every fleet agent posts as the same App, so
        # that App's verdict on that App's pull request counts.
        trusted = reviewer == FLEET_AUTHOR or association in TRUSTED_ASSOCIATIONS
        counts = trusted and (
            author is None or _normalized_login(author) != reviewer or reviewer == FLEET_AUTHOR
        )
        # An approval comes only from the fleet App or a human the base lists
        # in human_reviewers: any agent holding the owner's own gh login could
        # post a human one. A HOLD from a trusted human still counts.
        if (counts and verdict == "APPROVE" and reviewer != FLEET_AUTHOR
                and reviewer not in policy.get("humans", [])):
            EXCLUDED_APPROVALS.append(login)
            return False
        return counts
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
    """``(time, APPROVE|HOLD, head named, description)`` for every counting verdict.

    A HOLD's description is ``held by <signer> at <comment|review> <id>``:
    the ``reviewer:`` signer line when the body has one, else the login."""
    verdicts: list[tuple[datetime | None, str, str, str]] = []
    for review in reviews:
        login = (review.get("user") or {}).get("login")
        state = str(review.get("state") or "").upper()
        verdict = {"APPROVED": "APPROVE", "CHANGES_REQUESTED": "HOLD"}.get(state)
        if not isinstance(login, str) or verdict is None:
            continue
        if _reviewer_counts(login, review.get("author_association"), policy, author, verdict,
                            (review.get("user") or {}).get("type") == "Bot", review.get("body")):
            named = str(review.get("commit_id") or "").lower()
            what = (_held_by(login, review.get("body"), "review", review.get("id"))
                    if verdict == "HOLD" else
                    f"a {state} review by {login} at {review.get('submitted_at')}")
            verdicts.append((_timestamp(review.get("submitted_at")), verdict, named, what))
    for comment in comments:
        login = (comment.get("user") or {}).get("login")
        body = comment.get("body")
        found = marker_verdict(body) if isinstance(body, str) else None
        if not isinstance(login, str) or found is None:
            continue
        if _reviewer_counts(login, comment.get("author_association"), policy, author, found[0],
                            (comment.get("user") or {}).get("type") == "Bot", body):
            verdict, named = found
            what = (_held_by(login, body, "comment", comment.get("id"))
                    if verdict == "HOLD" else
                    f"`{verdict} head:{named[:12]}` by {login} at {comment.get('created_at')}")
            verdicts.append((_timestamp(comment.get("created_at")), verdict, named, what))
    return verdicts


def _held_by(login: str, body: Any, kind: str, record_id: Any) -> str:
    """``held by <signer> at <kind> <id>`` for one counting HOLD."""
    signed = _SIGNER_LINE.findall(body) if isinstance(body, str) else []
    signer = signed[-1].lower() if signed else login
    return f"held by {signer} at {kind} {record_id if record_id is not None else '(no id)'}"


def _latest_on(
    verdicts: list[tuple[datetime | None, str, str, str]], head: str
) -> tuple[datetime | None, str, str, str] | None:
    """The deciding verdict on ``head``: any HOLD, else the newest APPROVE.

    A HOLD is final for its head, so no APPROVE naming the same head, earlier
    or later, from any reviewer, outranks it. Same as src/head_approval.rs
    ``latest_verdict``."""
    on_head = [entry for entry in verdicts if entry[2] == head]
    holds = [entry for entry in on_head if entry[1] == "HOLD"]
    if holds:
        return holds[0]
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
        wanted += (", from the fleet App (a `ghapp` verdict comment), or a human the base "
                   "lists in `[auto_merge] human_reviewers`")
        if policy.get("humans"):
            wanted += f" ({', '.join(policy['humans'])})"
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
        posted = (at.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
                  if at is not None and at.tzinfo is not None else "with no readable time")
        excluded = [entry for entry in verdicts if entry[2] == head and entry[1] == "APPROVE"]
        if excluded:
            HELD_EXCLUSIONS.append(f"excluded_approval:head {what}")
        return False, (
            f"a HOLD naming head {head[:12]} makes it un-approvable ({what}, posted {posted})"
            + (f"; ignored {len(excluded)} APPROVE(s) of the same head" if excluded else "")
            + "; push a new head that addresses the HOLD and have it reviewed")
    older = [entry for entry in verdicts if entry[0] is not None]
    stale = ""
    if older:
        _, _, _, what = max(older, key=lambda entry: entry[0])
        stale = f" The latest verdict names another head: {what}."
    return False, f"no reviewer verdict names head {head[:12]}; it needs {wanted}.{stale}"


def _deciding_approval_comment(
    head: str,
    author: str | None,
    comments: list[dict[str, Any]],
    reviews: list[dict[str, Any]],
    policy: dict[str, Any],
) -> dict[str, Any] | None:
    """The APPROVE comment deciding ``head``, when one does."""
    head = head.lower()
    latest = _latest_on(counting_verdicts(author, comments, reviews, policy), head)
    if latest is None or latest[1] != "APPROVE" or latest[0] is None:
        return None
    for comment in comments:
        login = (comment.get("user") or {}).get("login")
        body = comment.get("body")
        if (isinstance(login, str) and isinstance(body, str)
                and marker_verdict(body) == ("APPROVE", head)
                and _timestamp(comment.get("created_at")) == latest[0]
                and _reviewer_counts(login, comment.get("author_association"), policy, author,
                                     "APPROVE", (comment.get("user") or {}).get("type") == "Bot",
                                     body)):
            return comment
    return None


def _deciding_approval_body(
    head: str,
    author: str | None,
    comments: list[dict[str, Any]],
    reviews: list[dict[str, Any]],
    policy: dict[str, Any],
) -> str | None:
    """The body of the APPROVE comment deciding ``head``, when one does."""
    comment = _deciding_approval_comment(head, author, comments, reviews, policy)
    return comment.get("body") if comment is not None else None


def _comment_effective_time(comment: dict[str, Any]) -> str | None:
    """When a comment last said what it says now: the later of ``created_at``
    and ``updated_at``, so a verdict edited in after a claim window is judged
    at its edit time. ``None`` (no readable time) when either timestamp is
    present but unreadable or lacks a UTC offset."""
    created_raw = comment.get("created_at")
    updated_raw = comment.get("updated_at")
    created = _timestamp(created_raw)
    if created is None or created.tzinfo is None:
        return None
    if updated_raw is None:
        return created_raw
    updated = _timestamp(updated_raw)
    if updated is None or updated.tzinfo is None:
        return None
    return updated_raw if updated > created else created_raw


def deciding_approval(
    head: str,
    author: str | None,
    comments: list[dict[str, Any]],
    reviews: list[dict[str, Any]],
    policy: dict[str, Any],
) -> dict[str, Any]:
    """Which verdict approved ``head``, for the arms ledger: the comment id
    and its ``reviewer:`` signer, or the APPROVED review's id."""
    head = head.lower()
    comment = _deciding_approval_comment(head, author, comments, reviews, policy)
    if comment is not None:
        signed = _SIGNER_LINE.findall(comment.get("body") or "")
        return {"verdict_comment_id": comment.get("id"),
                "signer": signed[-1].lower() if signed else None, "approved_head": head,
                "approved_at": _comment_effective_time(comment)}
    for review in reviews:
        if (str(review.get("state") or "").upper() == "APPROVED"
                and str(review.get("commit_id") or "").lower() == head):
            return {"verdict_review_id": review.get("id"),
                    "signer": (review.get("user") or {}).get("login"), "approved_head": head,
                    "approved_at": review.get("submitted_at")}
    return {"approved_head": head}


def self_approval(
    head: str,
    author: str | None,
    comments: list[dict[str, Any]],
    reviews: list[dict[str, Any]],
    policy: dict[str, Any],
) -> str | None:
    """Why the approval deciding ``head`` cannot land it from this agent.

    Refused only when the deciding APPROVE comment is stamped with an agent id
    and this process carries the same ``SHIPYARD_AGENT_ID``. A shared session
    never refuses (see ``same_session_note``).
    """
    mine = agent_id()
    if mine is None:
        return None
    body = _deciding_approval_body(head, author, comments, reviews, policy)
    if body is not None and stamped_agent(body) == mine:
        return (f"the approval of head {head.lower()[:12]} was posted by this same agent "
                f"({host_name()}/{mine}); an agent cannot approve its own landing")
    return None


def same_session_note(
    head: str,
    author: str | None,
    comments: list[dict[str, Any]],
    reviews: list[dict[str, Any]],
    policy: dict[str, Any],
) -> str | None:
    """A warning when the deciding approval carries this process's session.

    Every agent of one Claude Code team shares the session, so this cannot
    tell the reviewer from the agent arming: it warns and flags the ledger,
    never refuses.
    """
    mine = session_id()
    if mine == UNATTRIBUTED:
        return None
    body = _deciding_approval_body(head, author, comments, reviews, policy)
    if body is None or stamped_session(body) != mine:
        return None
    return (f"the approval of head {head.lower()[:12]} carries this process's session "
            f"({mine}); allowed, because a session is shared by every agent of a team. "
            f"Set {AGENT_ENV} per agent to have the same agent refused")


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


def glob_match(pattern: str, path: str) -> bool:
    """Whether ``path`` matches ``pattern``: ``**`` is any number of whole path
    segments, ``*`` any run of characters within one segment, ``?`` one.
    Same as src/head_approval.rs ``glob_match``."""
    def segments(pats: list[str], parts: list[str]) -> bool:
        if not pats:
            return not parts
        if pats[0] == "**":
            return any(segments(pats[1:], parts[index:]) for index in range(len(parts) + 1))
        return bool(parts) and _segment_match(pats[0], parts[0]) and segments(pats[1:], parts[1:])

    return segments(pattern.split("/"), path.split("/"))


def _segment_match(pattern: str, text: str) -> bool:
    if not pattern:
        return not text
    if pattern[0] == "*":
        return any(_segment_match(pattern[1:], text[index:]) for index in range(len(text) + 1))
    return bool(text) and (pattern[0] in ("?", text[0])) and _segment_match(pattern[1:], text[1:])


def carry_excluded(paths: set[str] | list[str], excluded: list[str]) -> str | None:
    """Why a pull request changing ``paths`` never carries an approval, or None.

    An ``excluded`` entry starting with ``!`` is an allow (see carry_paths): a
    path it matches carries unless a PROTECTED_CARRY_PATHS glob matches it."""
    allowed = [pattern[1:] for pattern in excluded if pattern.startswith("!")]
    blocked = [pattern for pattern in excluded if not pattern.startswith("!")]
    for path in sorted(paths):
        pattern = next((pattern for pattern in blocked if glob_match(pattern, path)), None)
        if pattern is None:
            continue
        protected = any(glob_match(glob, path) for glob in PROTECTED_CARRY_PATHS)
        if not protected and any(glob_match(allow, path) for allow in allowed):
            continue
        return (f"it changes {path} ({pattern}), and a change there always needs a "
                "verdict naming its exact head")
    return None


def carry_paths(section: dict[str, Any]) -> list[str] | None:
    """The carry exclusions an ``[auto_merge]`` table sets: None when it sets
    none of carry_excluded_paths, carry_excluded_paths_extra and
    carry_excluded_paths_allow. The list is carry_excluded_paths (which
    replaces the defaults) or DEFAULT_CARRY_EXCLUDED_PATHS, then the extra
    globs, then each allow as a ``!`` entry (see carry_excluded). Raises
    GuardError for a value that is not a list of strings, a ``!`` outside the
    allow list, or an allow carry_allow_error refuses. Same as
    src/head_approval.rs ``carry_paths``."""
    def globs(key: str) -> list[str] | None:
        value = section.get(key)
        if value is None:
            return None
        if not (isinstance(value, list) and all(isinstance(path, str) for path in value)):
            raise GuardError(f"[auto_merge] {key} must be a list of globs")
        return list(value)

    replace = globs("carry_excluded_paths")
    extra = globs("carry_excluded_paths_extra")
    allow = globs("carry_excluded_paths_allow")
    if replace is None and extra is None and allow is None:
        return None
    for key, paths in (("carry_excluded_paths", replace), ("carry_excluded_paths_extra", extra)):
        bang = next((path for path in paths or [] if path.startswith("!")), None)
        if bang is not None:
            raise GuardError(f"[auto_merge] {key} entry {bang} may not start with !")
    out = (replace if replace is not None else list(DEFAULT_CARRY_EXCLUDED_PATHS)) + (extra or [])
    for entry in allow or []:
        why = carry_allow_error(entry)
        if why is not None:
            raise GuardError(f"[auto_merge] carry_excluded_paths_allow entry {entry} {why}")
        out.append(f"!{entry}")
    return out


def carry_allow_error(entry: str) -> str | None:
    """Why ``entry`` may not be a carry_excluded_paths_allow entry, or None. An
    entry is a repository path or ``<directory>/**``, under a
    DEFAULT_CARRY_EXCLUDED_PATHS glob, and overlapping no PROTECTED_CARRY_PATHS
    glob. A directory is not checked against a ``**/`` name glob, which
    carry_excluded enforces under it. Same as src/head_approval.rs
    ``carry_allow_error``."""
    tree = entry.endswith("/**")
    path = entry[:-3] if tree else entry
    segments = path.split("/")
    if any(segment in ("", ".", "..") for segment in segments) or any(
            char in path for char in "*?[\\!"):
        return "is not a repository path or <directory>/**"
    ancestors = ["/".join(segments[:count]) for count in range(1, len(segments) + 1)]
    if tree:
        covered = any(default.endswith("/**") and any(
            glob_match(default[:-3], ancestor) for ancestor in ancestors)
            for default in DEFAULT_CARRY_EXCLUDED_PATHS)
    else:
        covered = any(glob_match(default, path) for default in DEFAULT_CARRY_EXCLUDED_PATHS)
    if not covered:
        return "is not under a default carry exclusion; the allow list only re-includes defaults"
    for protected in PROTECTED_CARRY_PATHS:
        if tree and protected.startswith("**/"):
            continue
        parts = protected.split("/")
        head = parts[:len(segments)]
        if any(glob_match(protected, ancestor) for ancestor in ancestors) or (
                tree and len(parts) > len(segments) and "**" not in head
                and glob_match("/".join(head), path)):
            return f"overlaps {protected}, which always needs a verdict naming its exact head"
    return None


def carry_limits(approved: str, author: str | None, comments: list[dict[str, Any]],
                 reviews: list[dict[str, Any]], policy: dict[str, Any],
                 now: datetime | None = None) -> str | None:
    """Why the approval of ``approved`` may not carry at all, or None: its
    verdict has a ``carry: no`` line, or it is older than MAX_CARRY_AGE."""
    latest = _latest_on(counting_verdicts(author, comments, reviews, policy), approved.lower())
    at = latest[0] if latest is not None else None
    if at is None:
        return "the time of its verdict is unknown"
    if (now or datetime.now(timezone.utc)) - at > MAX_CARRY_AGE:
        return (f"its verdict is older than {int(MAX_CARRY_AGE.total_seconds() // 3600)}h "
                f"({at.strftime('%Y-%m-%dT%H:%M:%SZ')})")
    record = _deciding_approval_comment(approved, author, comments, reviews, policy)
    if record is None:
        record = next((review for review in reviews
                       if str(review.get("state") or "").upper() == "APPROVED"
                       and str(review.get("commit_id") or "").lower() == approved.lower()
                       and _timestamp(review.get("submitted_at")) == at), None)
    if record is not None and _NO_CARRY_LINE.search(str(record.get("body") or "")):
        return "its verdict says `carry: no`"
    return None


def merge_only_carry(repo: str, base: str, approved: str, head: str,
                     excluded: list[str] | tuple[str, ...] = DEFAULT_CARRY_EXCLUDED_PATHS,
                     record: dict[str, Any] | None = None) -> str | None:
    """``None`` when the approval of ``approved`` carries to ``head``; else why not.

    It carries only when every commit on the first-parent chain from ``head``
    back to ``approved`` is a two-parent merge whose merged parent is on
    ``base``, and every file the pull request
    changes has, at ``head``, the same blob and mode it had at ``approved``. A file the
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
        merges = 0
        while sha != approved.lower():
            merges += 1
            if merges > MAX_CARRY_MERGES:
                return (f"it would carry across more than {MAX_CARRY_MERGES} base merges; "
                        "the head needs a new verdict")
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
        excluded_path = carry_excluded(
            set(before) | set(after) | set(_rename_sources(before_compare).values())
            | set(_rename_sources(after_compare).values()), list(excluded))
        if excluded_path is not None:
            return excluded_path
        binary = binary_files(repo, before, approved)
        if binary:
            return f"the pull request changes a binary file ({binary[0]})"
        if sorted(before) != sorted(after):
            changed = sorted(set(before) ^ set(after))
            return f"the set of files the pull request changes moved ({changed[0]})"
        for name, (status, sha, _) in sorted(before.items()):
            if after[name][:2] != (status, sha):
                return f"{name} at {head[:12]} is not the blob approved at {approved[:12]}"
        before_sources = _rename_sources(before_compare)
        after_sources = _rename_sources(after_compare)
        for name in sorted(before):
            if before_sources.get(name) != after_sources.get(name):
                return (f"{name} is renamed from {after_sources.get(name) or 'nothing'} at "
                        f"{head[:12]}, from {before_sources.get(name) or 'nothing'} when approved")
        # The comparison carries no file mode, so a merge that only flips a
        # file's executable bit keeps its blob: read the modes themselves.
        present = sorted(name for name, (status, _, _) in before.items() if status != "removed")
        if present:
            approved_modes = tree_modes(repo, approved, present)
            head_modes = tree_modes(repo, head, present)
            for name in present:
                if approved_modes.get(name) != head_modes.get(name):
                    return (f"{name} at {head[:12]} does not keep the mode approved at "
                            f"{approved[:12]}")
        base_before = (before_compare.get("merge_base_commit") or {}).get("sha")
        base_after = (after_compare.get("merge_base_commit") or {}).get("sha")
        if not isinstance(base_before, str) or not isinstance(base_after, str):
            return "the merge bases are unreadable"
        if base_before != base_after:
            moved = run_real_gh(["api", f"repos/{repo}/compare/{base_before}...{base_after}"])
            if not isinstance(moved, dict):
                return "what the base changed is unreadable"
            # A rename counts under both names, on either side: a base edit to
            # a file the pull request renames from would be dropped by a merge
            # that keeps the approved destination.
            pr_paths = set(before) | set(before_sources.values()) | set(after_sources.values())
            both = sorted(pr_paths & _base_touched(moved))
            if both:
                return f"{both[0]} was changed by both the pull request and the base"
        if record is not None:
            record.update(kind="merge", approved=approved, head=head, old_base=base_before,
                          new_base=base_after, merges=merges, files=len(before))
    except GuardError as error:
        return f"the carry cannot be checked ({error})"
    return None


# Rollups: same rule as src/rollup.rs ``accept``. A rollup head has no verdict
# of its own; it lands only when every merge on its first-parent chain merges a
# member's exact approved head and nothing else changed.
ROLLUP_BRANCH_PREFIX = "shipyard-train/rollup-"
ROLLUP_MEMBER_CAP = 4
TRAIN_KILL_ENV = "SHIPYARD_TRAIN"
TRAIN_PAUSE_FILE = "train.pause"


def train_config(config_text: str | None) -> dict[str, Any]:
    """The base's ``[train]`` table with its defaults (off). A value of the wrong
    type or out of range raises GuardError: it never turns rollups on."""
    config: dict[str, Any] = {"enabled": False, "mode": "off", "landing": "merge-queue",
                              "max_members": ROLLUP_MEMBER_CAP, "exclude_paths": [],
                              "hot_files": [], "require_counted_approval": True}
    if config_text is None:
        return config
    try:
        import tomllib
        section = tomllib.loads(config_text).get("train")
    except (ImportError, ValueError) as error:
        raise GuardError(f"the base's [train] cannot be read: {error}") from error
    if section is None:
        return config
    if not isinstance(section, dict):
        raise GuardError("the base's [train] is not a table")
    for key in ("enabled", "require_counted_approval"):
        if key in section:
            if not isinstance(section[key], bool):
                raise GuardError(f"[train] {key} is not true or false")
            config[key] = section[key]
    for key, allowed in (("mode", ("off", "burst", "rollup")),
                         ("landing", ("merge-queue", "direct"))):
        if key in section:
            if section[key] not in allowed:
                raise GuardError(f"[train] {key} {section[key]!r} is not one of {allowed}")
            config[key] = section[key]
    if "max_members" in section:
        members = section["max_members"]
        if (not isinstance(members, int) or isinstance(members, bool)
                or not 1 <= members <= ROLLUP_MEMBER_CAP):
            raise GuardError(f"[train] max_members must be a whole number from 1 to "
                             f"{ROLLUP_MEMBER_CAP}")
        config["max_members"] = members
    for key in ("exclude_paths", "hot_files"):
        if key in section:
            value = section[key]
            if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
                raise GuardError(f"[train] {key} is not a list of strings")
            config[key] = value
    return config


def train_stopped(repo: str, kill: str | None, guards_dir: pathlib.Path | None) -> str | None:
    """Why this host's train is stopped for ``repo``: ``SHIPYARD_TRAIN`` is
    ``off``, ``0`` or ``false``, or ``off:<owner/repo>``, or a ``train.pause``
    file is in the guards directory. Same as src/merge_train.rs
    ``kill_switch``."""
    value = (kill or "").strip()
    if value:
        word, _, target = value.partition(":")
        if value.lower() in ("off", "0", "false") or (
                target and word.lower() == "off" and target.strip().lower() == repo.lower()):
            return f"{TRAIN_KILL_ENV}={value} stops the train on this host"
    if guards_dir is not None and (guards_dir / TRAIN_PAUSE_FILE).exists():
        return f"the train is paused ({guards_dir / TRAIN_PAUSE_FILE})"
    return None


def _compare_files(repo: str, range_: str) -> tuple[dict[str, tuple[str, str, bool]], str,
                                                       dict[str, str]]:
    compare = run_real_gh(["api", f"repos/{repo}/compare/{range_}"])
    if not isinstance(compare, dict):
        raise GuardError(f"compare/{range_} is unreadable")
    merge_base = (compare.get("merge_base_commit") or {}).get("sha")
    if not isinstance(merge_base, str):
        raise GuardError(f"compare/{range_}: no merge base")
    return _pr_files(compare, f"compare/{range_}"), merge_base.lower(), _rename_sources(compare)


def _touched(files: dict[str, Any], sources: dict[str, str]) -> set[str]:
    return set(files) | set(sources.values())


def _same_change(what: str, expected: dict[str, tuple[str, str, bool]],
                 expected_sources: dict[str, str], found: dict[str, tuple[str, str, bool]],
                 found_sources: dict[str, str]) -> str | None:
    differ = sorted(set(expected) ^ set(found))
    if differ:
        return f"{what} changes {differ[0]}, which the member does not, or misses it"
    for name, (status, sha, _) in sorted(expected.items()):
        found_status, found_sha, _ = found[name]
        if status != found_status or sha.lower() != found_sha.lower():
            return (f"{what} leaves {name} {found_status} at blob {found_sha[:12]}, not the "
                    f"member's approved {status} at {sha[:12]}")
        if expected_sources.get(name) != found_sources.get(name):
            return f"{what} renames {name} from somewhere else"
    return None


def member_review(repo: str, number: int, head: str, policy: dict[str, Any],
                  counted: bool) -> str | None:
    """Why rollup member ``#number`` at ``head`` does not count, or None: the
    same review rule as any landing, at its own exact head; a carried approval
    counts only when ``counted`` (``require_counted_approval``) is off."""
    pull = run_real_gh(["api", f"repos/{repo}/pulls/{number}"])
    if not isinstance(pull, dict):
        raise GuardError(f"member #{number} is unreadable")
    author = (pull.get("user") or {}).get("login")
    author = author if isinstance(author, str) else None
    comments = _rest_pages(f"repos/{repo}/issues/{number}/comments?per_page=100")
    reviews = _rest_pages(f"repos/{repo}/pulls/{number}/reviews?per_page=100")
    allowed, why = review_verdict(head, author, comments, reviews, policy)
    approved = head
    if not allowed:
        approved = approved_earlier_head(head, author, comments, reviews, policy)
        base = ((pull.get("base") or {}).get("ref"))
        if counted or approved is None or not isinstance(base, str):
            return f"member #{number}: {why}"
        refusal = (carry_limits(approved, author, comments, reviews, policy)
                   or merge_only_carry(repo, base, approved, head,
                                       policy.get("carry_excluded",
                                                  list(DEFAULT_CARRY_EXCLUDED_PATHS))))
        if refusal is not None:
            return f"member #{number}: {why} The approval of {approved[:12]} does not carry: {refusal}"
    own = self_approval(approved, author, comments, reviews, policy)
    if own is not None:
        return f"member #{number}: {own}"
    return None


def rollup_acceptance(repo: str, number: int, head: str, pull: dict[str, Any],
                      config: dict[str, Any], policy: dict[str, Any],
                      review: Any = member_review) -> tuple[str | None, list[dict[str, Any]]]:
    """``(refusal, members)``: whether rollup ``#number`` at ``head`` lands.

    Same rule as src/rollup.rs ``accept``: (a) each member head is the current
    head of an open pull request on the base whose own review counts at that
    head; (b) the first-parent chain back to the base commit is only merges
    whose second parents are those heads, each changing exactly its member's
    files to the member's blobs; (c) members change disjoint files, the head
    keeps every member blob and mode, and the base touched no member file
    since the member's merge base nor since the rollup was formed; (d) no
    member changes an excluded path, and no two a hot one."""
    members: list[dict[str, Any]] = []
    if not (config["enabled"] and config["mode"] == "rollup"):
        return ('the base does not take rollups ([train] enabled = true, mode = "rollup")',
                members)
    stopped = train_stopped(repo, os.environ.get(TRAIN_KILL_ENV), _guards_dir())
    if stopped is not None:
        return stopped, members
    head = head.lower()
    if not str((pull.get("head") or {}).get("ref") or "").startswith(ROLLUP_BRANCH_PREFIX):
        return f"PR #{number}'s branch is not a {ROLLUP_BRANCH_PREFIX}* rollup", members
    head_repo = (((pull.get("head") or {}).get("repo") or {}).get("full_name") or "")
    if str(head_repo).lower() != repo.lower():
        return f"PR #{number}'s rollup branch is not in {repo}", members
    base = str((pull.get("base") or {}).get("ref") or "")
    union, base_sha, union_sources = _compare_files(repo, f"{base}...{head}")
    ref = run_real_gh(["api", f"repos/{repo}/git/ref/heads/{base}"])
    tip = str(((ref or {}).get("object") or {}).get("sha") or "").lower() \
        if isinstance(ref, dict) else ""
    if len(tip) != 40:
        raise GuardError(f"the tip of {base} is unreadable")
    links: list[tuple[str, str]] = []
    sha = head
    while sha != base_sha:
        if len(links) == config["max_members"]:
            return (f"the first-parent chain from {head[:12]} does not reach the base commit "
                    f"{base_sha[:12]} within {config['max_members']} merges"), members
        commit = run_real_gh(["api", f"repos/{repo}/commits/{sha}"])
        parents = [str(parent.get("sha")).lower() for parent in
                   (commit.get("parents") if isinstance(commit, dict) else None) or []]
        if len(parents) != 2:
            return (f"commit {sha[:12]} on the rollup's first-parent chain is not a two-parent "
                    "merge: a rollup is only merges of member heads"), members
        links.append((sha, parents[1]))
        sha = parents[0]
    if not links:
        return "the rollup merges no member", members
    links.reverse()
    seen: dict[str, int] = {}
    expected: dict[str, tuple[str, str, bool]] = {}
    expected_sources: dict[str, str] = {}
    previous = base_sha
    for merge, member_head in links:
        pulls = run_real_gh(["api", f"repos/{repo}/commits/{member_head}/pulls"])
        drafts = [item.get("number") for item in pulls or [] if isinstance(item, dict)
                  and item.get("state") == "open" and item.get("draft") is True
                  and str((item.get("head") or {}).get("sha") or "").lower() == member_head
                  ] if isinstance(pulls, list) else []
        if drafts:
            return f"member #{drafts[0]} is a draft; a draft never joins a rollup", members
        open_heads = [item.get("number") for item in pulls or [] if isinstance(item, dict)
                      and item.get("state") == "open"
                      and str((item.get("head") or {}).get("sha") or "").lower() == member_head
                      and (item.get("base") or {}).get("ref") == base
                      and item.get("number") != number] if isinstance(pulls, list) else []
        if len(open_heads) != 1 or not isinstance(open_heads[0], int):
            return (f"{member_head[:12]} is not the current head of exactly one open pull "
                    f"request on {base}"), members
        member = open_heads[0]
        if any(entry["pr"] == member for entry in members):
            return f"member #{member} is merged more than once", members
        refusal = review(repo, member, member_head, policy, config["require_counted_approval"])
        if refusal is not None:
            return refusal, members
        own, own_base, own_sources = _compare_files(repo, f"{base_sha}...{member_head}")
        files = _touched(own, own_sources)
        for name in sorted(files):
            if name in seen:
                return (f"members #{seen[name]} and #{member} both change {name}; rollup "
                        "members change disjoint files"), members
            seen[name] = member
        if own_base != base_sha:
            moved, _, moved_sources = _compare_files(repo, f"{own_base}...{base_sha}")
            both = sorted(_touched(moved, moved_sources) & files)
            if both:
                return (f"the base changed {both[0]} after member #{member}'s merge base "
                        f"{own_base[:12]}; the member must be updated and approved again before "
                        "it joins a rollup"), members
        step, _, step_sources = _compare_files(repo, f"{previous}...{merge}")
        refusal = _same_change(f"the rollup's merge {merge[:12]} of #{member}", own, own_sources,
                               step, step_sources)
        if refusal is not None:
            return refusal, members
        expected.update(own)
        expected_sources.update(own_sources)
        members.append({"pr": member, "head": member_head, "merge": merge,
                        "files": sorted(files)})
        previous = merge
    refusal = _same_change(f"the rollup head {head[:12]}", expected, expected_sources, union,
                           union_sources)
    if refusal is not None:
        return refusal, members
    # Checked apart, so a carry allow never re-includes a rollup exclusion.
    excluded_path = carry_excluded(set(seen), list(config["exclude_paths"])) or carry_excluded(
        set(seen), list(policy.get("carry_excluded", list(DEFAULT_CARRY_EXCLUDED_PATHS))))
    if excluded_path is not None:
        return f"a rollup member cannot land this way: {excluded_path}", members
    for pattern in config["hot_files"]:
        hot = sorted({owner for name, owner in seen.items() if glob_match(pattern, name)})
        if len(hot) > 1:
            return (f"members {hot} each change a hot file ({pattern}); at most one member of a "
                    "rollup may"), members
    present = sorted(name for name, (status, _, _) in union.items() if status != "removed")
    head_modes = tree_modes(repo, head, present) if present else {}
    for entry in members:
        names = [name for name in entry["files"] if name in present]
        modes = tree_modes(repo, entry["head"], names) if names else {}
        for name in names:
            if head_modes.get(name) != modes.get(name):
                return (f"{name} has mode {head_modes.get(name)} at the rollup head and "
                        f"{modes.get(name)} at member #{entry['pr']}'s approved head"), members
    if tip != base_sha:
        moved, _, moved_sources = _compare_files(repo, f"{base_sha}...{tip}")
        both = sorted(_touched(moved, moved_sources) & set(seen))
        if both:
            return (f"the base changed {both[0]}, which a member changes, after the rollup was "
                    f"formed on {base_sha[:12]}; form it again"), members
    return None, members


def rebase_carry_enabled() -> bool:
    """Whether this host carries approvals across a rebase (off unless turned on)."""
    switch = os.environ.get(REBASE_CARRY_ENV)
    if switch == "1":
        return True
    if switch == "0":
        return False
    return (_guards_dir() / REBASE_CARRY_FILE).is_file()


def patch_id(diff: str) -> str | None:
    """``git patch-id --stable`` of a diff (no repository needed); ``None`` when empty."""
    try:
        completed = subprocess.run(["git", "patch-id", "--stable"], input=diff,
                                   capture_output=True, text=True, check=False, timeout=60)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise GuardError(f"cannot run git patch-id: {error}") from error
    if completed.returncode != 0:
        raise GuardError(f"git patch-id failed: {completed.stderr.strip()[-200:]}")
    ids = [line.split()[0] for line in completed.stdout.splitlines() if line.strip()]
    if len(ids) > 1:
        raise GuardError("the diff read as more than one patch")
    return ids[0] if ids else None


def required_not_run_in_full(repo: str, base: str, head: str) -> str | None:
    """Why a required check on ``head`` did not run in full, or ``None``."""
    required = _required_contexts(run_real_gh, repo, base)
    if not required:
        return None
    runs = run_real_gh(["api", f"repos/{repo}/commits/{head}/check-runs?per_page=100"])
    listed = runs.get("check_runs") if isinstance(runs, dict) else None
    if not isinstance(listed, list) or runs.get("total_count") != len(listed):
        raise GuardError(f"the check runs of {head[:12]} are unreadable or partial")
    for context in required:
        named = [run for run in listed if isinstance(run, dict) and run.get("name") == context]
        if not named:
            continue
        latest = max(named, key=lambda run: run.get("id") if isinstance(run.get("id"), int) else 0)
        if latest.get("status") == "completed" and latest.get("conclusion") in NOT_RUN_IN_FULL:
            return f"required check `{context}` on {head[:12]} concluded `{latest['conclusion']}`"
        reused = _reused_receipt(repo, latest)
        if reused is not None:
            return f"required check `{context}` on {head[:12]} reused a receipt ({reused})"
    return None


def _reused_receipt(repo: str, run: dict[str, Any]) -> str | None:
    """The reason of a ``shipyard-receipt-decision`` annotation with verdict
    ``reuse`` on ``run`` (see docs/validation-signals.md), else ``None``."""
    run_id = run.get("id")
    output = run.get("output") if isinstance(run.get("output"), dict) else {}
    if not isinstance(run_id, int) or not output.get("annotations_count"):
        return None
    annotations = run_real_gh(["api", f"repos/{repo}/check-runs/{run_id}/annotations?per_page=100"])
    if not isinstance(annotations, list):
        raise GuardError(f"the annotations of check run {run_id} are unreadable")
    for annotation in annotations:
        if not isinstance(annotation, dict) or annotation.get("title") != RECEIPT_DECISION_TITLE:
            continue
        try:
            decision = json.loads(str(annotation.get("message") or ""))
        except json.JSONDecodeError as error:
            raise GuardError(f"check run {run_id} has an unparseable receipt decision") from error
        if isinstance(decision, dict) and decision.get("verdict") == "reuse":
            return str(decision.get("reason") or "no reason given")
    return None


# ---------------------------------------------------------------------------
# Tiered required checks: the twin of src/deferred_tier.rs.
# ---------------------------------------------------------------------------


def tier_policy(config_text: str | None) -> tuple[list[str], str | None]:
    """``(tiered_required_checks, tier_record_app)`` from the base's config;
    ``([], None)`` when no check is tiered."""
    if not config_text or "tiered_required_checks" not in config_text:
        return [], None
    try:
        import tomllib
    except ImportError as error:
        raise GuardError("the base config names tiered checks, and reading it needs "
                         "Python 3.11 or later (tomllib)") from error
    try:
        queue = tomllib.loads(config_text).get("queue")
    except tomllib.TOMLDecodeError as error:
        raise GuardError(f"the base config does not parse: {error}") from error
    names = queue.get("tiered_required_checks") if isinstance(queue, dict) else None
    if names is None:
        return [], None
    if (not isinstance(names, list)
            or not all(isinstance(name, str) and name.strip() for name in names)):
        raise GuardError("`queue.tiered_required_checks` is not a list of check names")
    if not names:
        return [], None
    app = queue.get("tier_record_app")
    app = app.strip() if isinstance(app, str) else ""
    if not app:
        raise GuardError("`queue.tiered_required_checks` is set but `queue.tier_record_app` "
                         "names no app")
    if app.lower() == ACTIONS_APP:
        raise GuardError(f"`queue.tier_record_app` may not be `{ACTIONS_APP}`: pull request "
                         "jobs act as that app and could rewrite its records")
    if app.lower() == FLEET_AUTHOR:
        raise GuardError(f"`queue.tier_record_app` may not be `{FLEET_AUTHOR}`: every agent "
                         "posts as the fleet App, so it could write a record")
    return names, app


def _check_runs(repo: str, head: str, api_json: Any) -> list[dict[str, Any]]:
    pages = api_json(["api", "--paginate", "--slurp",
                      f"repos/{repo}/commits/{head}/check-runs?per_page=100"])
    pages = pages if isinstance(pages, list) else [pages]
    listed: list[dict[str, Any]] = []
    total = None
    for page in pages:
        runs = page.get("check_runs") if isinstance(page, dict) else None
        if not isinstance(runs, list):
            raise GuardError(f"the check runs of {head[:12]} are unreadable")
        total = page.get("total_count")
        listed.extend(run for run in runs if isinstance(run, dict))
    if total != len(listed):
        raise GuardError(f"the check runs of {head[:12]} are partial")
    return listed


def _claim_receipt_app_matches(run: dict[str, Any], configured: str | int) -> bool:
    """Match the check-run's immutable GitHub App identity to the base policy."""
    app = run.get("app")
    if not isinstance(app, dict):
        return False
    if isinstance(configured, int):
        return app.get("id") == configured
    return app.get("slug") == configured


def parse_claim_receipt(
    run: dict[str, Any], repo: str, pr: int, head: str,
    context: str, app: str | int,
) -> dict[str, Any] | None:
    """Return a strict, head-bound durable reviewer-claim receipt.

    A receipt is a completed successful check run with the protected context
    and App identity. Its summary is a closed JSON object; unknown fields,
    malformed timestamps/epoch, or a mismatched repository/PR/head are all
    rejected. ``None`` means this run is not the configured receipt, while a
    malformed matching run raises ``GuardError`` so callers fail closed.
    """
    if run.get("name") != context:
        return None
    if not _claim_receipt_app_matches(run, app):
        raise GuardError(f"claim receipt `{context}` was produced by an unexpected GitHub App")
    # Failed, cancelled, queued and in-progress rows are historical or
    # revoked receipts. They do not certify the head; a current success is
    # selected below, while malformed successful rows still fail closed.
    if run.get("status") != "completed" or run.get("conclusion") != "success":
        return None
    output = run.get("output")
    summary = output.get("summary") if isinstance(output, dict) else None
    if not isinstance(summary, str) or not summary.strip():
        raise GuardError(f"claim receipt `{context}` has no JSON summary")
    try:
        value = json.loads(summary)
    except json.JSONDecodeError as error:
        raise GuardError(f"claim receipt `{context}` summary is not JSON") from error
    if not isinstance(value, dict):
        raise GuardError(f"claim receipt `{context}` summary is not an object")
    required = {"schema", "repo", "pr", "head_sha", "epoch", "reviewer",
                "claimed_at", "deadline_at"}
    if set(value) != required or value.get("schema") != CLAIM_RECEIPT_SCHEMA:
        raise GuardError(f"claim receipt `{context}` has an unexpected schema")
    # The producer writes the canonical (lower-cased) slug; GitHub slugs are
    # case-insensitive, so the binding is too.
    receipt_repo = value.get("repo")
    if (not isinstance(receipt_repo, str) or receipt_repo.lower() != repo.lower()
            or value.get("pr") != pr):
        raise GuardError(f"claim receipt `{context}` is bound to another pull request")
    receipt_head = value.get("head_sha")
    if not isinstance(receipt_head, str) or not re.fullmatch(r"[0-9a-fA-F]{40}", receipt_head):
        raise GuardError(f"claim receipt `{context}` has an invalid head")
    if receipt_head.lower() != head.lower():
        raise GuardError(f"claim receipt `{context}` names another head")
    if type(value.get("epoch")) is not int or value["epoch"] <= 0:
        raise GuardError(f"claim receipt `{context}` has an invalid epoch")
    reviewer = value.get("reviewer")
    if not isinstance(reviewer, str) or not reviewer.strip():
        raise GuardError(f"claim receipt `{context}` has no reviewer identity")
    for key in ("claimed_at", "deadline_at"):
        parsed = _timestamp(value.get(key))
        if parsed is None:
            raise GuardError(f"claim receipt `{context}` has an invalid {key} timestamp")
    deadline = _timestamp(value["deadline_at"])
    claimed = _timestamp(value["claimed_at"])
    now = datetime.now(timezone.utc)
    if claimed > now:
        raise GuardError(f"claim receipt `{context}` is future-dated")
    if deadline <= claimed:
        raise GuardError(f"claim receipt `{context}` has a non-forward deadline")
    # Expired receipts are normal history after a reviewer handoff. They do
    # not certify the head, but they must not poison a later valid receipt for
    # the same check name; malformed, future-dated and wrong-identity rows
    # still fail closed above.
    if deadline <= now:
        return None
    return value


def reviewer_claim_receipt(
    repo: str, pr: int, head: str, policy: dict[str, Any], approval: dict[str, Any],
) -> str | None:
    """Validate the optional configured claim receipt for a deciding approval."""
    context = policy.get("claim_receipt")
    app = policy.get("claim_app")
    if context is None and app is None:
        return None
    if not isinstance(context, str) or not context or app is None:
        raise GuardError("reviewer claim receipt policy is incomplete")
    listed = _check_runs(repo, head, run_real_gh)
    candidates = [run for run in listed if run.get("name") == context]
    # A newer check-run with the same protected name is the revocation or
    # replacement record for its older success. Real GitHub rows always carry
    # numeric ids; fixtures and older API shims may not, so retain the strict
    # exactly-one behavior when no ids are available.
    run_ids = [run.get("id") for run in candidates]
    if candidates and all(isinstance(run_id, int) for run_id in run_ids):
        latest = max(candidates, key=lambda run: run["id"])
        matches = [parse_claim_receipt(latest, repo, pr, head, context, app)]
    else:
        matches = [parse_claim_receipt(run, repo, pr, head, context, app)
                   for run in candidates]
    receipts = [receipt for receipt in matches if receipt is not None]
    if len(receipts) != 1:
        raise GuardError(f"expected exactly one valid reviewer claim receipt `{context}` on {head[:12]}")
    signer = approval.get("signer") if isinstance(approval, dict) else None
    if not isinstance(signer, str) or not signer.strip():
        raise GuardError("the deciding approval has no signer identity")
    if receipts[0]["reviewer"].lower() != signer.lower():
        raise GuardError("reviewer claim receipt identity does not match the deciding approval")
    approved_at = _timestamp(approval.get("approved_at")) if isinstance(approval, dict) else None
    if approved_at is None:
        raise GuardError("the deciding approval has no readable timestamp")
    claimed_at = _timestamp(receipts[0]["claimed_at"])
    deadline_at = _timestamp(receipts[0]["deadline_at"])
    if claimed_at is None or deadline_at is None or not (claimed_at <= approved_at < deadline_at):
        raise GuardError("the deciding approval was outside the reviewer claim window")
    return (f"reviewer claim epoch {receipts[0]['epoch']} by {receipts[0]['reviewer']} "
            f"consumed for head {head[:12]}")


def _app_slug(run: dict[str, Any]) -> Any:
    return (run.get("app") or {}).get("slug") if isinstance(run.get("app"), dict) else None


def _tier_records(listed: list[dict[str, Any]], check: str, run_id: int,
                  run_attempt: int, record_app: str) -> list[dict[str, Any]]:
    """The tier records bound to one run attempt of ``check``."""
    records = []
    for record in listed:
        if (record.get("name") != TIER_RECORD_PREFIX + check
                or _app_slug(record) != record_app or record.get("status") != "completed"):
            continue
        output = record.get("output") if isinstance(record.get("output"), dict) else {}
        try:
            summary = json.loads(str(output.get("summary") or ""))
        except json.JSONDecodeError:
            continue
        if (isinstance(summary, dict) and summary.get("schema") == TIER_RECORD_SCHEMA
                and summary.get("check") == check and summary.get("run_id") == run_id
                and summary.get("run_attempt") == run_attempt
                and isinstance(summary.get("tier"), str) and summary["tier"].strip()):
            records.append(summary)
    return records


def checks_scope(log: str) -> str:
    """The ``Checks`` scope of a job's token, from the TOKEN_PERMISSIONS_HEADER
    group of its log; ``none`` when the group does not list it. Raises
    :class:`GuardError` when the group is missing, never closes, or holds a
    line that is not ``<Scope>: read|write|none``. Same as src/deferred_tier.rs."""
    def text(line: str) -> str:
        line = line.rstrip("\r")
        stamp, _, rest = line.partition(" ")
        return rest if rest and stamp.endswith("Z") and "T" in stamp else line

    lines = iter(text(line) for line in log.splitlines())
    if not any(line.removeprefix("##[group]").strip() == TOKEN_PERMISSIONS_HEADER
               for line in lines):
        raise GuardError(f"its log has no `{TOKEN_PERMISSIONS_HEADER}` group")
    checks = None
    for line in lines:
        line = line.strip()
        if line == "##[endgroup]":
            return checks or "none"
        scope, colon, level = line.partition(":")
        scope, level = scope.strip(), level.strip().lower()
        if not colon or not scope or level not in ("read", "write", "none"):
            raise GuardError(f"its `{TOKEN_PERMISSIONS_HEADER}` group has an unparseable "
                             f"line `{line}`")
        if scope.lower() == "checks":
            if checks is not None:
                raise GuardError(f"its `{TOKEN_PERMISSIONS_HEADER}` group lists `Checks` twice")
            checks = level
    raise GuardError(f"its `{TOKEN_PERMISSIONS_HEADER}` group never closes")


def _tested_merge(repo: str, pr: int, base: str, head: str, record: dict[str, Any],
                  api_json: Any) -> str | None:
    """Why a full-lane record does not prove it tested this pull request's
    merge ref at the current head, or ``None`` when it does.

    All of these must hold: ``tested_ref`` is ``refs/pull/<n>/merge`` for this
    pull request; ``tested_sha``'s parents are exactly ``[merged_base, head]``,
    in GitHub's order (base first, head second); and ``merged_base`` is on the
    base branch. A bare branch, a merge of an older head, the reversed order,
    another ref, or a record missing any of the three fields is not. Same as
    src/deferred_tier.rs ``tested_merge``.
    """
    def is_sha(value: Any) -> bool:
        return isinstance(value, str) and re.fullmatch(r"[0-9a-fA-F]{40}", value) is not None

    tested = record.get("tested_sha")
    if not is_sha(tested):
        return "its record names no `tested_sha`"
    if tested.lower() == head.lower():
        return f"it tested the bare branch at {head[:12]}, not a merge with the base"
    merge_ref = f"refs/pull/{pr}/merge"
    tested_ref = record.get("tested_ref")
    if not isinstance(tested_ref, str):
        return f"its record names no `tested_ref` ({merge_ref})"
    if tested_ref != merge_ref:
        return f"it tested `{tested_ref}`, not this pull request's `{merge_ref}`"
    merged_base = record.get("merged_base")
    if not is_sha(merged_base):
        return "its record names no `merged_base`"
    try:
        commit = api_json(["api", f"repos/{repo}/commits/{tested}"])
    except GuardError as error:
        return f"tested commit {tested[:12]} is unreadable: {error}"
    parents = [parent.get("sha") for parent in (commit.get("parents") or [])
               if isinstance(parent, dict) and isinstance(parent.get("sha"), str)
               ] if isinstance(commit, dict) else []
    if len(parents) != 2:
        return (f"tested commit {tested[:12]} has {len(parents)} parent(s), so it is not "
                "a merge")
    first, second = parents
    # GitHub's refs/pull/<n>/merge puts the base first and the head second.
    if second.lower() != head.lower():
        if first.lower() == head.lower():
            return (f"tested commit {tested[:12]} has the head as its first parent, the "
                    f"reverse of GitHub's merge ref, so it is not {merge_ref}")
        return (f"tested commit {tested[:12]} is not a merge of the current head {head[:12]} "
                f"(its parents are {first[:12]} and {second[:12]}): a stale merge")
    if first.lower() != merged_base.lower():
        return (f"its record names `merged_base` {merged_base[:12]} but tested commit "
                f"{tested[:12]} merged {first[:12]}")
    try:
        compare = api_json(["api", f"repos/{repo}/compare/{merged_base}...{base}"])
    except GuardError as error:
        return f"whether {merged_base[:12]} is on `{base}` is unreadable: {error}"
    if isinstance(compare, dict) and compare.get("status") in ("identical", "ahead"):
        return None
    return f"tested commit {tested[:12]} merged {merged_base[:12]}, which is not on `{base}`"


def tier_blockers(repo: str, head: str, tiered: list[str], record_app: str,
                  pr: int, base: str,
                  api_json: Any = None, api_text: Any = None) -> list[dict[str, Any]]:
    """What holds ``head`` back for each tiered check, as ``{check, blocker, ...}``.

    The tier is never read from annotations, which the code under test can
    print. Per tiered check, its latest check run on the head must be a
    completed GitHub Actions job, and the tier records bound to that job's run
    attempt, created by ``record_app`` (never the Actions app, whose records a
    pull request job could PATCH), must exist and none may say ``deferred``. Each
    must show it tested this pull request's merge at the current head
    (:func:`_tested_merge`). Then, as defence in depth, the job's own token may
    not have been able to write check runs. Raises
    :class:`GuardError` when the check runs cannot be read in full.
    """
    api_json = api_json or run_real_gh
    api_text = api_text or run_real_gh_text
    listed = _check_runs(repo, head, api_json)
    latest: dict[str, dict[str, Any]] = {}
    for run in listed:
        name, run_id = run.get("name"), run.get("id")
        if isinstance(name, str) and isinstance(run_id, int):
            if name not in latest or run_id > latest[name]["id"]:
                latest[name] = run
    blockers = []
    for check in tiered:
        found = {"check": check, "head": head, "pr": pr}
        run = latest.get(check)
        if run is None:
            blockers.append({**found, "blocker": "pending", "status": "missing"})
            continue
        if run.get("status") != "completed":
            blockers.append({**found, "blocker": "pending",
                             "status": run.get("status") or "unknown"})
            continue
        if _app_slug(run) != ACTIONS_APP:
            blockers.append({**found, "blocker": "unreadable", "reason": (
                "it is not a GitHub Actions job, so no tier record can be bound to it")})
            continue
        try:
            job = api_json(["api", f"repos/{repo}/actions/jobs/{run['id']}"])
        except GuardError as error:
            blockers.append({**found, "blocker": "unreadable",
                             "reason": f"job {run['id']} unreadable: {error}"})
            continue
        run_id = job.get("run_id") if isinstance(job, dict) else None
        attempt = job.get("run_attempt") if isinstance(job, dict) else None
        if not isinstance(run_id, int) or not isinstance(attempt, int):
            blockers.append({**found, "blocker": "unreadable",
                             "reason": f"job {run['id']} names no run_id and run_attempt"})
            continue
        records = _tier_records(listed, check, run_id, attempt, record_app)
        if not records:
            blockers.append({**found, "blocker": "missing_record", "run_id": run_id,
                             "run_attempt": attempt})
            continue
        deferred = next((record for record in records
                         if record["tier"].strip().lower() == DEFERRED_TIER), None)
        if deferred is not None:
            blockers.append({**found, "blocker": "deferred", "record": deferred})
            continue
        untested = next(((record, why) for record in records
                         if (why := _tested_merge(repo, pr, base, head, record, api_json))),
                        None)
        if untested is not None:
            blockers.append({**found, "blocker": "untested_merge", "record": untested[0],
                             "reason": untested[1]})
            continue
        # Read only when the records would allow: a refusal needs no log.
        try:
            level = checks_scope(api_text(["api", f"repos/{repo}/actions/jobs/{run['id']}/logs"]))
        except GuardError as error:
            blockers.append({**found, "blocker": "unreadable",
                             "reason": f"job {run['id']}: {error}"})
            continue
        if level not in ("read", "none"):
            blockers.append({**found, "blocker": "checks_writable", "level": level})
    return blockers


def _explain_blocker(blocker: dict[str, Any]) -> tuple[str, str]:
    """``(why, fix)`` for one tier blocker; same text as src/deferred_tier.rs."""
    check, head = blocker["check"], blocker["head"][:12]
    kind = blocker["blocker"]
    if kind == "pending" and blocker["status"] == "missing":
        why = f"tiered required check `{check}` has not run on {head}"
    elif kind == "pending":
        why = f"tiered required check `{check}` on {head} is {blocker['status']}, not completed"
    elif kind == "deferred":
        record = blocker["record"]
        runs_in = record.get("full_suite_runs_in")
        why = (f"required check `{check}` on {head} ran only the deferred tier (selector "
               f"{record.get('selector') or 'unknown'}"
               + (f"; full suite runs in {runs_in}" if runs_in else "")
               + "); its success is not a full-suite result")
    elif kind == "untested_merge":
        why = (f"the full lane of tiered required check `{check}` on {head} did not test this "
               f"pull request's merge with its base ({blocker['reason']})")
    elif kind == "checks_writable":
        why = (f"the job of tiered required check `{check}` on {head} ran with "
               f"`checks: {blocker['level']}`, so it could have rewritten its own tier record")
    elif kind == "missing_record":
        why = (f"tiered required check `{check}` on {head} has no `{TIER_RECORD_PREFIX}{check}` "
               f"record for run {blocker['run_id']} attempt {blocker['run_attempt']}, so its "
               "tier is unproven")
    else:
        why = f"the tier of required check `{check}` on {head} cannot be read ({blocker['reason']})"
    if kind == "pending":
        fix = (f"wait for `{check}` to complete; the steward's native-arm backstop arms the "
               "head once it is green and its tier is full")
    elif kind in ("deferred", "untested_merge"):
        record, pr = blocker["record"], blocker["pr"]
        workflow = record.get("full_lane_workflow")
        what = (f"`{workflow}`" if isinstance(workflow, str) and workflow.strip()
                else f"the workflow of `{check}`")
        fix = (f"dispatch {what} with `{FULL_LANE_PR_INPUT}={pr}` as the fleet App and wait "
               "for the full lane")
    elif kind == "checks_writable":
        fix = (f"give the `{check}` job `checks: read` or `checks: none` in its workflow "
               "permissions and rerun it")
    elif kind == "missing_record":
        fix = (f"the workflow publishes a `{TIER_RECORD_PREFIX}{check}` check run for this run "
               "attempt (docs/ghapp-guards.md)")
    else:
        fix = "read it again once GitHub answers"
    return why, fix


def deferred_tier_refusal(targets: list[dict[str, Any]], repo: str | None) -> bool:
    """Print and return whether a tiered required check holds a target back.

    No override exists: the correct path (have the full lane run) is always
    available, and a deferred head is never ready to land.
    """
    refused: list[tuple[str, dict[str, Any] | None]] = []
    for target in targets:
        target_repo = target.get("repo") or repo
        number, head, base = target.get("pr"), target.get("head"), target.get("base")
        label = f"PR #{number}"
        if (not target_repo or not isinstance(head, str) or not head or not base
                or not isinstance(number, int)):
            refused.append((f"{label}: cannot tell which head and base would land, so its "
                            "test tier cannot be checked", None))
            continue
        try:
            tiered, record_app = tier_policy(base_config_text(target_repo, base))
            found = (tier_blockers(target_repo, head, tiered, record_app, number, base)
                     if tiered and record_app else [])
        except GuardError as error:
            refused.append((f"{label}: cannot read the test tier of its tiered required checks "
                            f"({error}); refusing to land a head that may be untested", None))
            continue
        for blocker in found:
            why, fix = _explain_blocker(blocker)
            refused.append((f"{label}: {why}", {
                "what": why, "cause": f"tier_{blocker['blocker']}",
                "evidence": f"https://github.com/{target_repo}/pull/{number}",
                "command": fix, "safe_to_auto_apply": False,
                "verify": (f"the latest `{blocker['check']}` run on {head[:12]} is completed and "
                           f"its `{TIER_RECORD_PREFIX}{blocker['check']}` record is not "
                           f"`{DEFERRED_TIER}`, and the same landing command passes this guard"),
            }))
    if not refused:
        return False
    print(
        f"queue-arm-guard: refusing to arm or merge a head whose tiered required checks have "
        f"not proven their full lane: {' | '.join(message for message, _ in refused)} "
        "Each `queue-arm-guard: fix:` line below says what to do next.",
        file=sys.stderr,
    )
    for _, fix in refused:
        if fix is not None:
            print(f"queue-arm-guard: fix: {json.dumps(fix, sort_keys=True)}", file=sys.stderr)
    return True


def _same_patch(old_diff: str, new_diff: str, record: dict[str, Any]) -> str | None:
    """Condition 1: the diff's ``git patch-id --stable`` did not change."""
    old_id, new_id = patch_id(old_diff), patch_id(new_diff)
    record["patch_id"] = old_id
    if old_id is None or old_id != new_id:
        return f"the diff's patch-id changed ({(old_id or 'none')[:12]} -> {(new_id or 'none')[:12]})"
    return None


# Git calls a file binary when a NUL byte appears in its first 8000 bytes.
BINARY_PROBE_BYTES = 8000


def _raw_head(repo: str, commit: str, path: str) -> bytes | None:
    """The first ``BINARY_PROBE_BYTES`` of ``path`` at ``commit``, by a ranged
    read of its raw URL, or ``None`` when the raw URL cannot be read (the API
    ignores ``Range``, the contents endpoint included; raw.githubusercontent.com
    honours it).

    This helps public repositories only: gh sends no ``Authorization`` header
    to raw.githubusercontent.com, so for a private repository the read fails
    and the caller falls back to the full blob read with its size check. That
    fallback is correct; it just saves nothing there.

    Raises GuardError when the read answers but returns other than exactly
    ``min(BINARY_PROBE_BYTES, size)`` bytes: a short read is never text.
    """
    from urllib.parse import quote
    real_gh = os.environ.get("GHAPP_REAL_GH", "/opt/homebrew/bin/gh")
    url = f"https://raw.githubusercontent.com/{repo}/{commit}/{quote(path)}"
    try:
        completed = subprocess.run(
            [real_gh, "api", "-i", "-H", f"Range: bytes=0-{BINARY_PROBE_BYTES - 1}", url],
            check=False, capture_output=True, timeout=60,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if completed.returncode != 0:
        return None
    raw = completed.stdout
    split = raw.find(b"\r\n\r\n")
    head, body = ((raw[:split], raw[split + 4:]) if split >= 0
                  else raw.partition(b"\n\n")[::2])
    lines = head.decode("latin-1").splitlines()
    status = lines[0].split()[1] if lines and len(lines[0].split()) > 1 else ""
    headers = {key.strip().lower(): value.strip()
               for key, _, value in (line.partition(":") for line in lines[1:])}
    if status == "206":
        match = re.fullmatch(r"bytes \d+-\d+/(\d+)", headers.get("content-range", ""))
        if match is None:
            raise GuardError(f"cannot tell whether {path} is binary: the ranged read has no size")
        size = int(match.group(1))
    elif status == "200":
        size = int(headers.get("content-length", -1))
    else:
        return None
    if len(body) != min(BINARY_PROBE_BYTES, size) and not (status == "200" and len(body) == size):
        raise GuardError(f"cannot tell whether {path} is binary: the read returned {len(body)} "
                         f"of {min(BINARY_PROBE_BYTES, size)} bytes")
    return body[:BINARY_PROBE_BYTES]


def blob_is_binary(repo: str, sha: str, commit: str | None = None, path: str | None = None) -> bool:
    """Whether blob ``sha`` is binary by git's rule (a NUL byte early on).

    GitHub omits a comparison's ``patch`` for a binary file, but also for a
    text file too large to show (a one-line JSON fixture of hundreds of KB),
    so a missing patch alone does not say binary. With ``commit`` and
    ``path`` only the first 8000 bytes are read, which works for public
    repositories; otherwise, or when that read is unavailable (any private
    repository), the whole blob is read and its length must equal its
    reported size. Raises GuardError when the blob cannot be read whole:
    unknown is a refusal, never "binary" and never text.
    """
    import base64
    if commit and path:
        head = _raw_head(repo, commit, path)
        if head is not None:
            return b"\0" in head
    try:
        blob = run_real_gh(["api", f"repos/{repo}/git/blobs/{sha}"])
    except GuardError as error:
        raise GuardError(f"cannot tell whether blob {sha[:12]} is binary: {error}") from error
    if not isinstance(blob, dict) or blob.get("encoding") != "base64":
        raise GuardError(f"cannot tell whether blob {sha[:12]} is binary: it is unreadable")
    try:
        data = base64.b64decode(str(blob.get("content") or ""))
    except ValueError as error:
        raise GuardError(f"cannot tell whether blob {sha[:12]} is binary: {error}") from error
    if not isinstance(blob.get("size"), int) or len(data) != blob["size"]:
        raise GuardError(f"cannot tell whether blob {sha[:12]} is binary: {len(data)} bytes "
                         f"read of {blob.get('size')}")
    return b"\0" in data[:BINARY_PROBE_BYTES]


def binary_files(repo: str, files: dict[str, tuple[str, str, bool]],
                 commit: str | None = None) -> list[str]:
    """The files a comparison lists that are binary: those with no text patch
    whose blob is binary. A file with a patch is text. ``commit`` is where
    the listed files that were not removed can be read by path."""
    return sorted(
        name for name, (status, sha, has_patch) in files.items()
        if not has_patch and blob_is_binary(
            repo, sha, commit if status != "removed" else None, name)
    )


def _rename_sources(compare: dict[str, Any]) -> dict[str, str]:
    """``filename -> previous_filename`` for every rename a comparison lists."""
    return {
        item["filename"]: item["previous_filename"]
        for item in compare.get("files") or []
        if isinstance(item, dict) and isinstance(item.get("filename"), str)
        and isinstance(item.get("previous_filename"), str)
    }


def _same_files(before: dict[str, tuple[str, str, bool]], after: dict[str, tuple[str, str, bool]],
                head: str, before_sources: dict[str, str] | None = None,
                after_sources: dict[str, str] | None = None) -> str | None:
    """Condition 2: the same files, with the same status and, for a rename,
    the same source."""
    if sorted(before) != sorted(after):
        changed = sorted(set(before) ^ set(after))
        return f"the set of files the pull request changes moved ({changed[0]})"
    for name in sorted(before):
        if before[name][0] != after[name][0]:
            return f"{name} is {after[name][0]} at {head[:12]}, {before[name][0]} when approved"
    before_sources, after_sources = before_sources or {}, after_sources or {}
    for name in sorted(before):
        if before_sources.get(name) != after_sources.get(name):
            return (f"{name} is renamed from {after_sources.get(name) or 'nothing'} at {head[:12]}, "
                    f"from {before_sources.get(name) or 'nothing'} when approved")
    return None


def _base_touched(moved: dict[str, Any]) -> set[str]:
    """Every path the base changed between the two merge bases, counting both
    names of a rename, so a file it renamed or deleted is visible."""
    files = moved.get("files")
    if not isinstance(files, list):
        raise GuardError("the base's changes list no files")
    if len(files) >= COMPARE_FILE_CAP:
        raise GuardError(f"the base's changes list {len(files)} files, GitHub's cap; it may be partial")
    touched: set[str] = set()
    for item in files:
        if not isinstance(item, dict) or not isinstance(item.get("filename"), str):
            raise GuardError("the base's changes list a file with no name")
        touched.add(item["filename"])
        if isinstance(item.get("previous_filename"), str):
            touched.add(item["previous_filename"])
    return touched


def _reapplies_exactly(repo: str, approved: str, head: str,
                       before: dict[str, tuple[str, str, bool]],
                       after: dict[str, tuple[str, str, bool]],
                       base_touched: set[str], rename_sources: set[str] | None = None) -> str | None:
    """Condition 3: re-applying the approved diff onto the new merge base gives
    exactly the new head's tree. That is proved, not computed: the base touched
    (changed, renamed or deleted) none of the pull request's files between the
    merge bases, and every one of them has, at the new head, the blob and mode
    it was approved with. No binary change is accepted."""
    binary = binary_files(repo, before, approved)
    if binary:
        return f"the pull request changes a binary file ({binary[0]})"
    # A rename's source is the pull request's file too: a base edit to it
    # would be dropped by a rebase that keeps the approved destination.
    both = sorted((set(before) | set(after) | (rename_sources or set())) & base_touched)
    if both:
        return f"{both[0]} was changed, renamed or deleted by the base between the merge bases"
    for name, (status, approved_blob, _) in sorted(before.items()):
        if name not in after:
            continue  # condition 2's finding
        if status != "removed" and after[name][1] != approved_blob:
            return f"{name} at {head[:12]} is not the blob approved at {approved[:12]}"
    present = sorted(name for name, (status, _, _) in before.items()
                     if status != "removed" and name in after)
    if present:
        approved_modes = tree_modes(repo, approved, present)
        head_modes = tree_modes(repo, head, present)
        for name in present:
            if approved_modes.get(name) != head_modes.get(name):
                return f"{name} at {head[:12]} does not keep the mode approved at {approved[:12]}"
    return None


def rebase_carry(repo: str, base: str, approved: str, head: str,
                 excluded: list[str] | tuple[str, ...] = DEFAULT_CARRY_EXCLUDED_PATHS
                 ) -> tuple[str | None, dict[str, Any]]:
    """``(None, record)`` when the approval of ``approved`` carries across a
    rebase to ``head``; else ``(why not, record)``.

    Four conditions, each checked on its own and every failing one named:

    1. the pull request's diff has the same ``git patch-id --stable`` at both
       heads (each against its own merge base);
    2. it changes the same files, with the same status;
    3. re-applying the approved diff onto the new merge base gives exactly the
       new head's tree: the base touched (changed, renamed or deleted) none of
       the pull request's files between the merge bases, the new merge base
       descends from the old one, and every file keeps its approved blob and
       mode, so no conflict was resolved by hand;
    4. no required check on the new head concluded ``skipped`` or ``neutral``
       or reused a receipt: the carry covers the review, never the checks.

    Conditions 1 and 3 must agree: a patch-id that changed refuses even when
    condition 3 holds.
    Any binary change, or anything unreadable, refuses.
    """
    record: dict[str, Any] = {"repo": repo, "approved": approved, "head": head}
    try:
        before_compare = run_real_gh(["api", f"repos/{repo}/compare/{base}...{approved}"])
        after_compare = run_real_gh(["api", f"repos/{repo}/compare/{base}...{head}"])
        if not isinstance(before_compare, dict) or not isinstance(after_compare, dict):
            return "the pull request's files are unreadable", record
        old_base = (before_compare.get("merge_base_commit") or {}).get("sha")
        new_base = (after_compare.get("merge_base_commit") or {}).get("sha")
        if not isinstance(old_base, str) or not isinstance(new_base, str):
            return "the merge bases are unreadable", record
        record.update(old_base=old_base, new_base=new_base)
        moved = run_real_gh(["api", f"repos/{repo}/compare/{old_base}...{new_base}"])
        if not isinstance(moved, dict) or moved.get("status") not in ("ahead", "identical"):
            return f"the new merge base {new_base[:12]} does not descend from {old_base[:12]}", record
        before = _pr_files(before_compare, f"compare/{base}...{approved[:12]}")
        after = _pr_files(after_compare, f"compare/{base}...{head[:12]}")
        before_sources = _rename_sources(before_compare)
        after_sources = _rename_sources(after_compare)
        base_touched = _base_touched(moved) if moved.get("status") == "ahead" else set()
        record["files"] = len(before)
        excluded_path = carry_excluded(
            set(before) | set(after) | set(before_sources.values()) | set(after_sources.values()),
            list(excluded))
        if excluded_path is not None:
            return excluded_path, record
        diff_header = ["-H", "Accept: application/vnd.github.diff"]
        old_diff = run_real_gh_text(["api", *diff_header, f"repos/{repo}/compare/{old_base}...{approved}"])
        new_diff = run_real_gh_text(["api", *diff_header, f"repos/{repo}/compare/{new_base}...{head}"])
        reasons = [reason for reason in (
            _same_patch(old_diff, new_diff, record),
            _same_files(before, after, head, before_sources, after_sources),
            _reapplies_exactly(repo, approved, head, before, after, base_touched,
                               set(before_sources.values()) | set(after_sources.values())),
            required_not_run_in_full(repo, base, head),
        ) if reason is not None]
    except GuardError as error:
        return f"the rebase carry cannot be checked ({error})", record
    return ("; ".join(reasons) if reasons else None), record


def log_carry(record: dict[str, Any]) -> None:
    """Append one carry to the guards directory's carry log. Never raises."""
    entry = dict(record, at=datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
                 argv=sys.argv[1:], **writer_identity())
    entry.setdefault("kind", "rebase")
    try:
        directory = _guards_dir()
        directory.mkdir(parents=True, exist_ok=True)
        with (directory / REBASE_CARRY_LOG).open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(entry, sort_keys=True) + "\n")
    except OSError:
        pass


TREE_BATCH = 50


def tree_modes(repo: str, commit: str, names: list[str]) -> dict[str, int | None]:
    """Each file's git mode at ``commit`` (``None`` when absent), from its directory's tree."""
    owner, _, name = repo.partition("/")
    by_dir: dict[str, list[str]] = {}
    for path in names:
        by_dir.setdefault(posixpath.dirname(path), []).append(path)
    directories = sorted(by_dir)
    modes: dict[str, int | None] = {}
    for start in range(0, len(directories), TREE_BATCH):
        batch = directories[start:start + TREE_BATCH]
        fields = "".join(
            f"d{index}:object(expression:{json.dumps(commit + ':' + directory)})"
            "{...on Tree{entries{name mode}}}"
            for index, directory in enumerate(batch)
        )
        query = f"query{{repository(owner:{json.dumps(owner)},name:{json.dumps(name)}){{{fields}}}}}"
        value = run_real_gh(["api", "graphql", "-f", f"query={query}"])
        if not isinstance(value, dict) or value.get("errors") or not isinstance(
                (value.get("data") or {}).get("repository"), dict):
            raise GuardError(f"the trees at {commit[:12]} are unreadable")
        repository = value["data"]["repository"]
        for index, directory in enumerate(batch):
            tree = repository.get(f"d{index}") or {}
            entries = {entry.get("name"): entry.get("mode")
                       for entry in tree.get("entries") or [] if isinstance(entry, dict)}
            for path in by_dir[directory]:
                mode = entries.get(posixpath.basename(path))
                modes[path] = mode if isinstance(mode, int) else None
    return modes


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
        "or `sha=` on the REST merge."
    )
    if not isinstance(bound, str) or not bound:
        return f"the request does not bind the head it lands; bind it to {head} with {hint}"
    if bound.lower() != head.lower():
        return f"the request binds {bound[:12]}, but the head is {head[:12]}"
    return None


def review_fix(cause: str, what: str, repo: str, number: int, head: str,
               command: str, safe: bool) -> dict[str, Any]:
    """The actionable fields a review refusal carries (printed as one JSON line)."""
    return {
        "what": what,
        "evidence": f"https://github.com/{repo}/pull/{number}",
        "cause": cause,
        "command": command,
        "safe_to_auto_apply": safe,
        "verify": (f"`ghapp api repos/{repo}/pulls/{number} --jq .head.sha` prints {head}, and "
                   "the same landing command passes this guard"),
    }


def _note_same_session(classification: dict[str, Any], label: str, repo: str,
                       note: str | None) -> None:
    """Print a same-session warning and flag the target for the arms ledger."""
    if note is None:
        return
    classification["same_session_approval"] = True
    print(f"queue-arm-guard: warning: {label} ({repo}): {note}", file=sys.stderr)


def review_gate(classification: dict[str, Any], repo: str | None, dry_run: bool = False
                ) -> tuple[bool, str, dict[str, Any] | None]:
    """``(allowed, message, fix)``: the review-before-arm rule for one target,
    with every human approval it excluded printed and kept on the target for
    the arms ledger. ``dry_run`` judges a carry without appending it to the
    carry log: read-only callers (the review-verdict status) pass it, so only
    a landing ever logs a carry. Same as src/head_approval.rs ``dry_run``."""
    EXCLUDED_APPROVALS.clear()
    HELD_EXCLUSIONS.clear()
    result = _review_gate(classification, repo, dry_run)
    if HELD_EXCLUSIONS:
        classification["held_exclusions"] = sorted(set(HELD_EXCLUSIONS))
        print(f"queue-arm-guard: note: PR #{classification.get('pr')}: "
              f"{'; '.join(classification['held_exclusions'])}: a HOLD makes its head "
              "un-approvable, so only a new head can be approved", file=sys.stderr)
    excluded = sorted(set(EXCLUDED_APPROVALS))
    if excluded:
        classification["excluded_approvals"] = excluded
        print(f"queue-arm-guard: note: PR #{classification.get('pr')}: ignored the approval "
              f"of {', '.join(excluded)}: only the fleet App's verdict (signed by an allowed "
              "reviewer where the host lists signers), or a login the base lists in "
              "`[auto_merge] human_reviewers`, approves", file=sys.stderr)
    return result


def _review_gate(classification: dict[str, Any], repo: str | None, dry_run: bool = False
                 ) -> tuple[bool, str, dict[str, Any] | None]:
    """``(allowed, message, fix)``: the review-before-arm rule for one target.

    ``fix`` says, for a refusal it can name, what to do next.
    """
    repo = classification.get("repo") or repo
    number = classification.get("pr")
    head = classification.get("head")
    label = f"PR #{number}"
    if not repo or not isinstance(number, int) or not isinstance(head, str):
        return False, f"{label}: cannot tell which head would land, so its review cannot be checked", None
    if not host_review_default():
        # Off is inert: nothing is read, so no config, Python version or
        # GitHub answer can change what an arm does on a host that opted out.
        return True, (f"review before arm is OFF on this host ({HOST_OPT_OUT_FILE}: "
                      f"{opt_out_reason()})"), None
    read_pull = f"ghapp api repos/{repo}/pulls/{number} --jq .head.sha"
    try:
        pull = run_real_gh(["api", f"repos/{repo}/pulls/{number}"])
        base = ((pull.get("base") or {}).get("ref")) if isinstance(pull, dict) else None
        if not isinstance(base, str) or not base:
            raise GuardError("the pull request carries no base branch")
        config_text = base_config_text(repo, base)
        policy = review_policy(config_text, repo, True)
        if not policy["required"]:
            return True, f"{repo} does not require a review before arming", None
        live_head = ((pull.get("head") or {}).get("sha") or "").lower()
        if live_head != head.lower():
            return False, (
                f"{label}: the head moved while the guard read it ({head[:12]} -> "
                f"{live_head[:12]}); read it again"
            ), review_fix("head_moved", f"{label} moved from {head[:12]} to {live_head[:12]}",
                          repo, number, live_head, read_pull, True)
        author = (pull.get("user") or {}).get("login")
        author = author if isinstance(author, str) else None
        comments = _rest_pages(f"repos/{repo}/issues/{number}/comments?per_page=100")
        reviews = _rest_pages(f"repos/{repo}/pulls/{number}/reviews?per_page=100")
    except GuardError as error:
        return False, (
            f"{label}: cannot read what its review requires ({error}); refusing to land it unreviewed"
        ), review_fix("unreadable", f"{label}'s review state could not be read: {error}",
                      repo, number, head, read_pull, True)
    unbound = _binding_refusal(classification, head)
    if unbound is not None:
        return False, f"{label} ({repo}): {unbound}", review_fix(
            "unbound", f"the landing request for {label} does not bind head {head}",
            repo, number, head,
            f"repeat the same command with `--match-head-commit {head}` (gh pr merge), "
            f"`expectedHeadOid: \"{head}\"` (GraphQL) or `sha={head}` (REST)", False)
    allowed, why = review_verdict(head, author, comments, reviews, policy)
    if allowed:
        own = self_approval(head, author, comments, reviews, policy)
        if own is not None:
            return False, f"{label} ({repo}): {own}", review_fix(
                "self_approval", own, repo, number, head,
                f"another agent reviews head {head} and posts "
                f"`verdict: APPROVE head:{head}` on https://github.com/{repo}/pull/{number}", False)
        try:
            receipt_note = reviewer_claim_receipt(
                repo, number, head, policy,
                deciding_approval(head, author, comments, reviews, policy),
            )
        except GuardError as error:
            return False, f"{label} ({repo}): reviewer claim receipt refused ({error})", review_fix(
                "unreviewed", f"the reviewer claim receipt for {label} could not be proved: {error}",
                repo, number, head, read_pull, True)
        if receipt_note is not None:
            why = f"{why}; {receipt_note}"
        _note_same_session(classification, label, repo,
                           same_session_note(head, author, comments, reviews, policy))
        classification["approval"] = deciding_approval(head, author, comments, reviews, policy)
        return True, f"{label} ({repo}): {why}", None
    cause = "hold" if "is HOLD" in why or "HOLD naming" in why else "unreviewed"
    head_ref = str((pull.get("head") or {}).get("ref") or "")
    if cause == "unreviewed" and head_ref.startswith(ROLLUP_BRANCH_PREFIX) and _latest_on(
            counting_verdicts(author, comments, reviews, policy), head.lower()) is None:
        try:
            config = train_config(config_text)
            refusal, members = (rollup_acceptance(repo, number, head, pull, config, policy)
                                if config["enabled"] and config["mode"] == "rollup"
                                else (None, None))
        except GuardError as error:
            refusal, members = f"it cannot be checked ({error})", []
        if refusal is None and members:
            classification["approval"] = {"approved_head": head.lower(), "rollup": [
                {"pr": entry["pr"], "head": entry["head"]} for entry in members]}
            named = ", ".join("#{}@{}".format(entry["pr"], entry["head"][:12]) for entry in members)
            return True, (
                f"{label} ({repo}): a rollup of {named}: "
                "every merge on its first-parent chain merges a member's exact approved head, "
                "and every file keeps that member's blob and mode"
            ), None
        if refusal is not None:
            why += f" As a rollup it does not land: {refusal}."
    approved = approved_earlier_head(head, author, comments, reviews, policy)
    if approved is not None:
        excluded = policy.get("carry_excluded", list(DEFAULT_CARRY_EXCLUDED_PATHS))
        limited = (f"the base's carry configuration is refused ({policy['carry_error']})"
                   if policy.get("carry_error")
                   else carry_limits(approved, author, comments, reviews, policy))
        merge_record: dict[str, Any] = {}
        refusal = limited or merge_only_carry(repo, base, approved, head, excluded, merge_record)
        own = self_approval(approved, author, comments, reviews, policy)
        if own is not None:
            return False, f"{label} ({repo}): {own}", review_fix(
                "self_approval", own, repo, number, head,
                f"another agent reviews head {head} and posts "
                f"`verdict: APPROVE head:{head}` on https://github.com/{repo}/pull/{number}", False)
        carry_note = same_session_note(approved, author, comments, reviews, policy)
        if refusal is None:
            _note_same_session(classification, label, repo, carry_note)
            classification["approval"] = deciding_approval(approved, author, comments, reviews,
                                                           policy)
            if not dry_run:
                log_carry(dict(merge_record, repo=repo, pr=number, **{
                    key: value for key, value in classification["approval"].items()
                    if key in ("verdict_comment_id", "verdict_review_id", "signer")}))
            return True, (
                f"{label} ({repo}): the approval of {approved[:12]} carries to {head[:12]}: "
                "every commit since is a merge of the base and every file the pull request "
                "changes keeps its approved blob and mode"
            ), None
        if limited is None and cause == "unreviewed" and rebase_carry_enabled():
            rebase_refusal, record = rebase_carry(repo, base, approved, head, excluded)
            if rebase_refusal is None:
                record["pr"] = number
                if not dry_run:
                    log_carry(record)
                print(f"ghapp: {label} ({repo}): approval of {approved[:12]} carried across a "
                      f"rebase to {head[:12]} (logged to {REBASE_CARRY_LOG})", file=sys.stderr)
                _note_same_session(classification, label, repo, carry_note)
                classification["approval"] = deciding_approval(approved, author, comments,
                                                               reviews, policy)
                return True, (
                    f"{label} ({repo}): the approval of {approved[:12]} carries across the rebase "
                    f"to {head[:12]}: same patch-id, same files, the base touched none of them "
                    "and each keeps its approved blob and mode, and no required check was skipped "
                    "or reused a receipt. The carry "
                    "covers the review only; required checks still run on the new head"
                ), None
            refusal += f"; across a rebase: {rebase_refusal}"
        why += f" The approval of {approved[:12]} does not carry: {refusal}."
        cause = "approval_does_not_carry" if cause == "unreviewed" else cause
    what = {
        "hold": f"a trusted HOLD names {label}'s head {head[:12]}, so it is un-approvable",
        "approval_does_not_carry": (f"{label}'s approval of {approved[:12] if approved else ''} "
                                    f"does not carry to head {head[:12]}"),
        "unreviewed": f"no trusted verdict names {label}'s head {head[:12]}",
    }[cause]
    if cause == "hold":
        return False, f"{label} ({repo}): {why}", review_fix(
            cause, what, repo, number, head,
            f"the author pushes a new head to https://github.com/{repo}/pull/{number} that "
            "addresses the HOLD; a reviewer then reviews that new head. No APPROVE of the "
            f"held head {head} counts", False)
    command = (f"a reviewer agent reviews head {head} and posts, through `ghapp` (the fleet "
               f"App), a comment ending `verdict: APPROVE head:{head}` on "
               f"https://github.com/{repo}/pull/{number}; a human approval counts only for a "
               "login the base lists in `[auto_merge] human_reviewers`")
    # Approving is the reviewer's act; an agent landing its own head must not do it.
    return False, f"{label} ({repo}): {why}", review_fix(
        cause, what, repo, number, head, command, False)


def audit_override(message: str, **fields: Any) -> None:
    """Append one line to the guards' override audit log. Never raises."""
    directory = _guards_dir()
    record = {
        "at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "override": REVIEW_OVERRIDE_ENV,
        "refusal": message,
        "argv": sys.argv[1:],
        "cwd": os.getcwd(),
        **writer_identity(),
        **fields,
    }
    try:
        directory.mkdir(parents=True, exist_ok=True)
        with (directory / OVERRIDES_LOG).open("a", encoding="utf-8") as handle:
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


def _target(response: Any, action: str, bound: Any) -> dict[str, Any]:
    """One classified landing target, with the base its required checks are read from."""
    return {**classify_pr_queue_state(response), "action": action, "base": _base_ref(response),
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
        return [_target(response, action, bound[-1] if bound else None)]
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
            _target(graphql(PR_BY_ID_QUERY, {"id": node}), action, bound)
            for action, node, bound in landing_mutations(document, graphql_variables(args))
        ]
    return None


def internal_targets(args: list[str]) -> list[dict[str, Any]] | None:
    """What a Shipyard-internal landing binds, read from ``args`` alone.

    Shipyard judged it already, so no GitHub call is spent here: the ledger
    gets the repository, pull request (or GraphQL node), head and verb the
    request itself names. ``shipyard who`` matches an entry without a pull
    request number by its repository and head.
    """
    parser = _require_parser()
    repo_flag = parser.option_values(args, {"--repo", "-R"})
    repo = repo_flag[-1] if repo_flag else (os.environ.get("GH_REPO") or None)
    if len(args) >= 2 and args[0] == "pr" and args[1] == "merge":
        if "--disable-auto" in args:
            return None
        selector, flag_repo = pr_merge_target(args)
        repo = flag_repo or repo
        number: int | None = None
        if selector:
            url = re.search(r"github\.com/([^/]+/[^/]+)/pull/([1-9][0-9]*)", selector)
            if url:
                repo, number = url.group(1), int(url.group(2))
            elif re.fullmatch(r"#?[1-9][0-9]*", selector):
                number = int(selector.lstrip("#"))
        bound = parser.option_values(args[2:], {"--match-head-commit"})
        action = "arm" if "--auto" in args else "merge"
        return [{"repo": repo, "pr": number, "head": bound[-1].lower() if bound else None,
                 "action": action}]
    if args and args[0] == "api":
        endpoint, query = parser.api_target(args)
        rest = re.fullmatch(r"repos/([^/]+/[^/]+)/pulls/([1-9][0-9]*)/merge", endpoint)
        if rest:
            sha = rest_fields(args).get("sha")
            return [{"repo": rest.group(1), "pr": int(rest.group(2)),
                     "head": sha.lower() if isinstance(sha, str) else None, "action": "merge"}]
        if endpoint != "graphql":
            return None
        document = "\n".join([parser.graphql_document(args), *query.get("query", [])])
        compact = "".join(document.split()).lower()
        if not any(name in compact for name in (*ARM_MUTATIONS, *MERGE_MUTATIONS)):
            return None
        return [{"repo": repo, "pr": None, "node": node,
                 "head": bound.lower() if isinstance(bound, str) else None, "action": action}
                for action, node, bound in landing_mutations(document, graphql_variables(args))]
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
    return [_target(response, "merge", fields.get("sha"))]


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


def review_override(message: str) -> bool:
    """The review rule has no override: a ``GHAPP_ALLOW_UNREVIEWED_ARM=1``
    attempt is refused, said so and audited. Always False."""
    if os.environ.get(REVIEW_OVERRIDE_ENV) == "1":
        audit_override(message, honoured=False,
                       declined="the review rule has no override")
        print(f"queue-arm-guard: {REVIEW_OVERRIDE_ENV}=1 is not honoured: the review rule "
              "has no override; the head lands only with its review verdict.",
              file=sys.stderr)
    return False


def review_refusal(targets: list[dict[str, Any]], repo: str | None) -> bool:
    """Print and return whether an unreviewed head refuses the arm.

    Nothing overrides it; a GHAPP_ALLOW_UNREVIEWED_ARM=1 attempt is audited.
    """
    refused = [
        (message, fix)
        for allowed, message, fix in (review_gate(target, repo) for target in targets)
        if not allowed
    ]
    if not refused:
        return False
    message = " | ".join(message for message, _ in refused)
    review_override(message)
    print(
        f"queue-arm-guard: refusing to arm or merge an unreviewed head: {message} "
        "Each `queue-arm-guard: fix:` line below says what to do next.",
        file=sys.stderr,
    )
    for _, fix in refused:
        if fix is not None:
            print(f"queue-arm-guard: fix: {json.dumps(fix, sort_keys=True)}", file=sys.stderr)
    return True


def review_only(args: list[str]) -> int:
    """The review rule alone, for a landing Shipyard is about to make itself.

    Shipyard judges queue state on its own paths, so only ``review_refusal``
    runs, exactly as it does for an agent's request. Every judged request is recorded in the arms ledger, allowed or
    refused; a request naming no head it can classify is refused.
    """
    repo = os.environ.get("GH_REPO") or None
    review = "on" if host_review_default() else f"off: {opt_out_reason()}"
    try:
        targets = arm_request(args)
    except GuardError as error:
        print(f"queue-arm-guard: refusing to arm or merge: the request cannot be judged: {error}",
              file=sys.stderr)
        try:
            unjudged = internal_targets(args)
        except Exception:  # noqa: BLE001 - the ledger never gates
            unjudged = None
        record_arms(unjudged or [{"repo": repo, "action": "unknown"}], repo, "refused", review)
        return 1
    if targets is None:
        return 0
    refused = review_refusal(targets, repo)
    record_arms(targets, repo, "refused" if refused else "allowed", review)
    return 1 if refused else 0


def shipyard_nonce(purpose: str) -> bool:
    """Whether Shipyard issued this call's one-shot nonce for ``purpose``
    (the removal guard's ``consume_nonce``); without that parser, it did not."""
    try:
        return PARSER is not None and bool(PARSER.consume_nonce(purpose))
    except Exception:  # noqa: BLE001 - an unreadable record claims nothing
        return False


def main(args: list[str]) -> int:
    if os.environ.get(REVIEW_ONLY_ENV) == "1":
        if shipyard_nonce("review-only"):
            return review_only(args)
        print(f"queue-arm-guard: ignoring {REVIEW_ONLY_ENV}=1: it carries no valid one-shot "
              "nonce from a call Shipyard made, so every rule applies.", file=sys.stderr)
    internal = os.environ.get("SHIPYARD_INTERNAL_QUEUE_MUTATION") == "1"
    if internal and not shipyard_nonce("internal-arm"):
        print("queue-arm-guard: ignoring SHIPYARD_INTERNAL_QUEUE_MUTATION=1: it carries no "
              "valid one-shot nonce from a call Shipyard made, so it cannot claim "
              "Shipyard's authority.", file=sys.stderr)
    elif internal:
        # Shipyard's own arm and merge paths ran the review rule already
        # (queue_arm_gate.rs); record it so `shipyard who` can name them too.
        # Never refuse here, and never fail: parsing is best effort.
        try:
            record_arms(internal_targets(args), os.environ.get("GH_REPO") or None,
                        "allowed", "shipyard internal")
        except Exception:  # noqa: BLE001 - the ledger never gates
            pass
        return 0
    seen: dict[str, Any] = {}
    status = judge(args, seen)
    review = "on" if host_review_default() else f"off: {opt_out_reason()}"
    if review != "on":
        print(f"queue-arm-guard: note: review before arm is OFF on this host "
              f"({HOST_OPT_OUT_FILE}: {opt_out_reason()})", file=sys.stderr)
    record_arms(seen.get("targets"), os.environ.get("GH_REPO") or None,
                "allowed" if status == 0 else "refused", review)
    return status


def judge(args: list[str], seen: dict[str, Any]) -> int:
    """The guard's decision for one request; ``seen["targets"]`` gets what it judged."""
    override = os.environ.get("GHAPP_ALLOW_QUEUE_REARM") == "1"
    try:
        targets = arm_request(args)
        seen["targets"] = targets
    except GuardError as error:
        message = f"refusing ambiguous queue-arm request: {error}"
        if override:
            # Nothing names the head, so its review cannot be checked.
            review_override(message)
            message += "; it names no head, so its review cannot be checked"
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
    # Neither override stands in for a full-lane result or a review.
    if deferred_tier_refusal(targets, repo):
        return 1
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

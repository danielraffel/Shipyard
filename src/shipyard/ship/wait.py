"""Truth-condition evaluators for ``shipyard wait``.

Each evaluator takes a snapshot from ``gh`` and returns a deterministic
``TruthResult`` describing:

* ``matched`` — True if the wait condition is satisfied.
* ``observed`` — a JSON-serializable view of what was observed (populated
  into the ``OutputEnvelope.observed`` field).
* ``error`` — a caller-friendly sentinel when evaluation can't proceed
  (invalid PR number, rulesets detected, etc.).

Pure logic — no I/O. The CLI layer handles the actual ``gh`` invocation
(via ``wait_transport``) so these stay unit-testable against fixture
JSON.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

# Conclusion mapping mirrors docs/shipyard-wait-primitive-design-v4.md.
PASSING_CONCLUSIONS = frozenset({"SUCCESS", "NEUTRAL", "SKIPPED"})
FAILING_CONCLUSIONS = frozenset(
    {"FAILURE", "TIMED_OUT", "CANCELLED", "ACTION_REQUIRED", "STARTUP_FAILURE", "STALE"}
)
STILL_WAITING_STATES = frozenset({"QUEUED", "IN_PROGRESS", "PENDING"})


class UnsupportedScopeError(Exception):
    """Raised when a condition can't be evaluated because it sits in a
    governance surface we don't support yet (rulesets, merge queues).

    The CLI surfaces this as exit code 7.
    """


class InvalidInputError(Exception):
    """Raised when input refers to a missing PR, release, or run.

    The CLI surfaces this as exit code 5.
    """


@dataclass(frozen=True)
class TruthResult:
    matched: bool
    observed: dict[str, Any] = field(default_factory=dict)


# ---------------------------------------------------------------------------
# release


def evaluate_release(
    snapshot: dict[str, Any] | None,
    *,
    manifest: list[str] | None,
) -> TruthResult:
    """Matches when the release exists, isn't a draft, and every asset in
    the manifest has ``state == "uploaded"``. When ``manifest`` is None
    (or empty), any non-empty assets list counts as matched.

    ``snapshot`` is the ``gh api`` response for
    ``repos/{owner}/{repo}/releases/tags/{tag}``. Pass ``None`` when the
    release doesn't exist yet — returns ``matched=False`` so callers can
    keep waiting.
    """
    if snapshot is None:
        return TruthResult(matched=False, observed={"exists": False})
    if snapshot.get("draft") is True:
        return TruthResult(
            matched=False,
            observed={"exists": True, "draft": True},
        )
    assets_raw = snapshot.get("assets") or []
    assets: list[dict[str, Any]] = []
    for asset in assets_raw:
        if isinstance(asset, dict):
            assets.append(
                {
                    "name": str(asset.get("name", "")),
                    "state": str(asset.get("state", "")),
                    "size": int(asset.get("size", 0) or 0),
                }
            )
    observed: dict[str, Any] = {
        "exists": True,
        "draft": False,
        "tag_name": snapshot.get("tag_name", ""),
        "assets": assets,
    }
    if manifest:
        by_name = {a["name"]: a for a in assets}
        missing: list[str] = []
        not_uploaded: list[str] = []
        for name in manifest:
            entry = by_name.get(name)
            if entry is None:
                missing.append(name)
            elif entry["state"] != "uploaded":
                not_uploaded.append(name)
        observed["manifest"] = list(manifest)
        observed["missing"] = missing
        observed["not_uploaded"] = not_uploaded
        matched = not missing and not not_uploaded
        return TruthResult(matched=matched, observed=observed)
    # No manifest: match as soon as any asset is uploaded.
    has_upload = any(a["state"] == "uploaded" for a in assets)
    return TruthResult(matched=has_upload, observed=observed)


# ---------------------------------------------------------------------------
# pr --state green


def evaluate_pr_green(snapshot: dict[str, Any] | None) -> TruthResult:
    """Matches when every classic-branch-protection required check on the
    PR's current head SHA has conclusion ∈ {SUCCESS, NEUTRAL, SKIPPED}.

    ``snapshot`` is the ``gh pr view --json
    number,headRefOid,state,mergeable,statusCheckRollup,mergeStateStatus``
    response. Rulesets / merge-queue flavoured PRs raise
    :class:`UnsupportedScopeError` — the caller maps that to exit code 7.
    """
    if snapshot is None:
        raise InvalidInputError("PR not found")
    merge_state = str(snapshot.get("mergeStateStatus") or "").upper()
    # GitHub's mergeStateStatus returns NOT_SUPPORTED/BLOCKED variants
    # when rulesets / merge-queue gate required checks. Detect on the
    # distinctive name-rich rulesets flag rather than a plain BLOCKED
    # (BLOCKED is also used for branch-protection-required checks that
    # simply haven't landed yet, which is our happy path).
    if "RULESET" in merge_state or merge_state == "MERGE_QUEUED":
        raise UnsupportedScopeError(
            "Rulesets / merge-queue governance isn't supported by "
            "`shipyard wait pr --state green` yet — see "
            "governance/profiles.py."
        )
    rollup = snapshot.get("statusCheckRollup") or []
    head_sha = str(snapshot.get("headRefOid") or "")
    required_entries: list[dict[str, Any]] = []
    advisory_entries: list[dict[str, Any]] = []
    all_required_pass = True
    any_still_waiting = False
    for entry in rollup:
        if not isinstance(entry, dict):
            continue
        name = str(entry.get("name") or entry.get("context") or "")
        state = str(entry.get("state") or "").upper()
        conclusion = str(entry.get("conclusion") or "").upper()
        required = bool(entry.get("isRequired", False))
        observed_entry = {
            "name": name,
            "state": state or None,
            "conclusion": conclusion or None,
            "required": required,
        }
        if not required:
            advisory_entries.append(observed_entry)
            continue
        required_entries.append(observed_entry)
        if state in STILL_WAITING_STATES and conclusion not in PASSING_CONCLUSIONS:
            any_still_waiting = True
            all_required_pass = False
            continue
        if conclusion in PASSING_CONCLUSIONS:
            continue
        # Terminal-but-not-passing → never-matching. Broadcast as
        # a non-match so waiters can exit 1 on --timeout (not 0).
        all_required_pass = False
    if not required_entries:
        # No required checks is a valid matched state only when
        # mergeable is clean; otherwise report as non-match so callers
        # keep polling.
        all_required_pass = str(snapshot.get("mergeable") or "").upper() == "MERGEABLE"
    matched = all_required_pass and not any_still_waiting
    return TruthResult(
        matched=matched,
        observed={
            "pr": snapshot.get("number"),
            "head_sha": head_sha,
            "merge_state_status": merge_state or None,
            "checks": required_entries,
            "advisory": advisory_entries,
        },
    )


# ---------------------------------------------------------------------------
# pr --state merged / closed


def evaluate_pr_state(
    snapshot: dict[str, Any] | None,
    *,
    target_state: str,
) -> TruthResult:
    """Matches when PR's state reaches ``merged`` or ``closed``."""
    if snapshot is None:
        raise InvalidInputError("PR not found")
    state = str(snapshot.get("state") or "").upper()
    merged = bool(snapshot.get("merged", False))
    observed = {
        "pr": snapshot.get("number"),
        "state": state,
        "merged": merged,
    }
    if target_state == "merged":
        return TruthResult(matched=merged, observed=observed)
    if target_state == "closed":
        return TruthResult(matched=state in {"CLOSED", "MERGED"}, observed=observed)
    raise InvalidInputError(f"unknown target state {target_state!r}")


# ---------------------------------------------------------------------------
# run


RUN_TERMINAL_STATUSES = frozenset({"completed"})


def evaluate_run(
    snapshot: dict[str, Any] | None,
    *,
    require_success: bool,
) -> TruthResult:
    """Matches when a workflow run reaches a terminal status.

    With ``--success`` the match additionally requires ``conclusion ==
    "success"``. Any other terminal conclusion is a fail-fast: we raise
    a :class:`RunFailedFastError` so the CLI can exit 4 rather than wait out
    the timeout on a run that will never pass.
    """
    if snapshot is None:
        raise InvalidInputError("run not found")
    status = str(snapshot.get("status") or "").lower()
    conclusion = str(snapshot.get("conclusion") or "").lower()
    observed = {
        "run_id": snapshot.get("databaseId") or snapshot.get("id"),
        "status": status,
        "conclusion": conclusion or None,
    }
    if status not in RUN_TERMINAL_STATUSES:
        return TruthResult(matched=False, observed=observed)
    if not require_success:
        return TruthResult(matched=True, observed=observed)
    if conclusion == "success":
        return TruthResult(matched=True, observed=observed)
    raise RunFailedFastError(observed)


class RunFailedFastError(Exception):
    """``wait run --success`` hit a terminal-but-wrong conclusion.

    Carries the ``observed`` dict so the CLI layer can still emit it
    in the JSON envelope before exiting 4.
    """

    def __init__(self, observed: dict[str, Any]) -> None:
        super().__init__(f"run terminal conclusion: {observed.get('conclusion')}")
        self.observed = observed

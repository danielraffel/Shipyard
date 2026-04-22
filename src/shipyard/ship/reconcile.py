"""Re-fetch PR CI state from GitHub and heal drifted ship-state files.

Webhook events update `dispatched_runs[].status` as they arrive. When
the daemon wasn't running (crashed, not yet spawned, or registered on
the wrong set of repos), those events are lost — the ship-state file
keeps whatever status the last-received event wrote. The macOS GUI
reads that file and keeps showing the stale status.

This module fetches the current `statusCheckRollup` for a PR via `gh`
and updates dispatched_runs to match what GitHub thinks is true right
now. The matching is deliberately forgiving (substring containment on
the check name) because GitHub workflow names vary across repos
(``Build and Test / windows (pull_request)`` vs. ``windows`` vs.
``Build / Windows (x64)``) and we only need to reconcile by target
identity.

Pure logic — no I/O. The CLI layer handles `gh` invocation and file
writes so this module stays unit-testable.
"""

from __future__ import annotations

from dataclasses import replace
from datetime import datetime, timezone

from shipyard.core.ship_state import DispatchedRun, ShipState


def _conclusion_to_run_status(conclusion: str | None, state: str | None) -> str:
    """Map a GitHub check's conclusion/state to our DispatchedRun.status
    vocabulary (pending | in_progress | completed | failed | cancelled).

    GitHub's fields:
      * state: QUEUED | IN_PROGRESS | COMPLETED | PENDING | …
      * conclusion (set once state==COMPLETED): SUCCESS | FAILURE |
        TIMED_OUT | CANCELLED | NEUTRAL | SKIPPED | ACTION_REQUIRED …
    """
    s = (state or "").upper()
    c = (conclusion or "").upper()
    # GitHub's statusCheckRollup returns `state: null` for legacy
    # commit-status checks and some CheckRun shapes once the check has
    # a conclusion. Treat "conclusion set, state unset" as completed.
    if s in {"QUEUED", "PENDING"}:
        return "pending"
    if s == "IN_PROGRESS":
        return "in_progress"
    if s != "COMPLETED" and not c:
        # No conclusion and no recognized state — don't guess.
        return ""
    if c in {"SUCCESS", "NEUTRAL", "SKIPPED"}:
        return "completed"
    if c == "CANCELLED":
        return "cancelled"
    # FAILURE, TIMED_OUT, ACTION_REQUIRED, STARTUP_FAILURE, STALE, ...
    return "failed"


def _match_check(run: DispatchedRun, checks: list[dict]) -> dict | None:
    """Find the GitHub check whose name best identifies this target.

    Matching rules, in priority order:
      1. Exact name match (check.name == run.target).
      2. Word-boundary containment (' mac ' in ' Build and Test / mac ').
      3. Case-insensitive substring (less selective, used as fallback).

    If multiple checks match, prefer the most-recently-updated one so
    we track re-runs after a failed attempt.
    """
    target_lc = run.target.lower()
    exact: list[dict] = []
    word_boundary: list[dict] = []
    substring: list[dict] = []
    for check in checks:
        name = str(check.get("name") or "")
        name_lc = name.lower()
        if name_lc == target_lc:
            exact.append(check)
            continue
        # Word-boundary: target appears as a whole token. Pads both sides
        # with separators so "macos" doesn't match "mac" and vice versa.
        padded = f" {name_lc.replace('/', ' ').replace('(', ' ').replace(')', ' ')} "
        if f" {target_lc} " in padded:
            word_boundary.append(check)
            continue
        if target_lc in name_lc:
            substring.append(check)
    pool = exact or word_boundary or substring
    if not pool:
        return None
    # Prefer the most-recent by completedAt/startedAt.
    def _key(c: dict) -> str:
        return str(c.get("completedAt") or c.get("startedAt") or "")
    return max(pool, key=_key)


def reconcile_ship_state(
    state: ShipState,
    status_check_rollup: list[dict],
    *,
    now: datetime | None = None,
) -> tuple[ShipState, list[str]]:
    """Produce a new ShipState with dispatched_runs healed to match
    what GitHub's statusCheckRollup reports. Returns the new state and
    a list of human-readable change descriptions for logging.

    Inputs:
      * ``state`` — the drifted ship-state we want to heal.
      * ``status_check_rollup`` — the raw list from
        ``gh pr view <n> --json statusCheckRollup`` (each entry is a
        dict with at least ``name``, ``state``, ``conclusion``).

    No matching check found → target keeps its old status (don't
    overwrite with uncertainty). Match found but status unchanged →
    no change recorded. Match found with different status → replaced.
    """
    now = now or datetime.now(timezone.utc)
    new_runs: list[DispatchedRun] = []
    new_evidence = dict(state.evidence_snapshot)
    changes: list[str] = []
    for run in state.dispatched_runs:
        match = _match_check(run, status_check_rollup)
        if match is None:
            new_runs.append(run)
            continue
        new_status = _conclusion_to_run_status(
            match.get("conclusion"), match.get("state")
        )
        if not new_status:
            new_runs.append(run)
            continue
        if new_status != run.status:
            changes.append(
                f"target={run.target!r}: {run.status!r} → {new_status!r} "
                f"(matched check {match.get('name')!r})"
            )
            new_runs.append(
                replace(run, status=new_status, updated_at=now)
            )
        else:
            new_runs.append(run)
        # Evidence snapshot is what the GUI actually renders (it
        # overwrites dispatched_run status when present, see
        # ShipStatePoller.swift:113). Mirror the healed run status
        # into evidence so the GUI reflects GitHub truth. Only
        # terminal statuses map cleanly — leave in_progress / pending
        # alone so we don't stomp a running indicator with stale
        # evidence.
        evidence_value = _run_status_to_evidence(new_status)
        if evidence_value is not None and new_evidence.get(run.target) != evidence_value:
            changes.append(
                f"evidence[{run.target!r}]: "
                f"{new_evidence.get(run.target)!r} → {evidence_value!r}"
            )
            new_evidence[run.target] = evidence_value
    new_state = ShipState(**{
        **state.__dict__,
        "dispatched_runs": new_runs,
        "evidence_snapshot": new_evidence,
    })
    return new_state, changes


def _run_status_to_evidence(run_status: str) -> str | None:
    """Project a DispatchedRun.status onto the evidence_snapshot
    vocabulary used by the GUI (``pass`` / ``fail`` / ``reused``).

    Non-terminal statuses return None so callers leave evidence alone.
    """
    if run_status == "completed":
        return "pass"
    if run_status in {"failed", "cancelled"}:
        return "fail"
    return None

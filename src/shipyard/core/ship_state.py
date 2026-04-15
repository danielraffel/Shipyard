"""Durable in-flight ship state for resume-after-interruption.

When `shipyard ship` is in progress, the ship flow holds a lot of
context in memory: which PR is being shipped, which cloud runs were
dispatched, which targets have produced evidence, and the merge
policy that was in effect when the ship started. Today, all of that
lives only in Python memory. When the session dies (laptop closed,
OS restart, agent crash), that knowledge is lost and the next
session has to re-dispatch from scratch.

This module persists the minimum set of facts needed to resume an
in-flight ship:

    <state_dir>/ship/<pr>.json

    {
        "schema_version": 1,
        "pr": 224,
        "repo": "danielraffel/pulp",
        "branch": "feature/foo",
        "base_branch": "main",
        "head_sha": "abc1234...",
        "dispatched_runs": [
            {"target": "cloud", "provider": "namespace",
             "run_id": "24446948064", "status": "in_progress",
             "started_at": "...", "updated_at": "..."}
        ],
        "evidence_snapshot": {"macos": "pass", "linux": "pending"},
        "policy_signature": "0123abcd",
        "attempt": 2,
        "created_at": "...",
        "updated_at": "..."
    }

After merge/cancel/final-fail, the file is moved to an `archive/`
subdirectory, keyed by PR + timestamp, so historical context is
preserved for `shipyard evidence` inspection without cluttering the
active state directory.

This store is intentionally separate from:
- EvidenceStore (core/evidence.py) — merge-gating evidence keyed by
  branch+target. That store answers "is this PR mergeable?"; this
  store answers "what was I doing when my session died?".
- Queue (core/queue.py) — task queue keyed by job. The queue's
  stale-job recovery handles "process died," but it does not know
  which cloud run IDs correspond to which targets. This store
  persists exactly that linkage so a fresh session can pick up
  without re-dispatching.
"""

from __future__ import annotations

import contextlib
import hashlib
import json
import os
import tempfile
from dataclasses import dataclass, field, replace
from datetime import datetime, timedelta, timezone
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from pathlib import Path


SCHEMA_VERSION = 1


@dataclass(frozen=True)
class DispatchedRun:
    """A single remote run dispatched during a ship.

    For cloud targets, `run_id` is the GitHub Actions run ID. For
    SSH targets, `run_id` is Shipyard's internal job ID. `provider`
    distinguishes between cloud providers (e.g., "namespace",
    "github-hosted") or SSH transports ("ssh", "ssh-windows"). The
    `status` field is the last observed status from the poller:
    "pending", "in_progress", "completed", "failed", "cancelled".
    """

    target: str
    provider: str
    run_id: str
    status: str
    started_at: datetime
    updated_at: datetime
    attempt: int = 1

    def to_dict(self) -> dict[str, Any]:
        return {
            "target": self.target,
            "provider": self.provider,
            "run_id": self.run_id,
            "status": self.status,
            "started_at": self.started_at.isoformat(),
            "updated_at": self.updated_at.isoformat(),
            "attempt": self.attempt,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> DispatchedRun:
        return cls(
            target=d["target"],
            provider=d["provider"],
            run_id=str(d["run_id"]),
            status=d["status"],
            started_at=datetime.fromisoformat(d["started_at"]),
            updated_at=datetime.fromisoformat(d["updated_at"]),
            attempt=d.get("attempt", 1),
        )


@dataclass
class ShipState:
    """Durable record of an in-flight ship for a single PR.

    A ShipState is mutable in memory — the ship flow updates it as
    events happen (dispatch, poll update, evidence record, merge).
    Callers should save through ShipStateStore after every
    significant update so a crash loses at most one event.

    Human-context fields (`pr_url`, `pr_title`, `commit_subject`)
    are best-effort metadata so that `shipyard ship-state list` and
    `show` are self-describing — the operator doesn't have to
    remember what PR #42 was about to decide whether to resume it.
    """

    pr: int
    repo: str
    branch: str
    base_branch: str
    head_sha: str
    policy_signature: str
    pr_url: str = ""
    pr_title: str = ""
    commit_subject: str = ""
    dispatched_runs: list[DispatchedRun] = field(default_factory=list)
    evidence_snapshot: dict[str, str] = field(default_factory=dict)
    attempt: int = 1
    created_at: datetime = field(
        default_factory=lambda: datetime.now(timezone.utc)
    )
    updated_at: datetime = field(
        default_factory=lambda: datetime.now(timezone.utc)
    )
    schema_version: int = SCHEMA_VERSION

    def touch(self) -> None:
        """Bump `updated_at` to now. Call after every mutation."""
        self.updated_at = datetime.now(timezone.utc)

    def upsert_run(self, run: DispatchedRun) -> None:
        """Insert or replace a DispatchedRun matching on (target, run_id)."""
        for i, existing in enumerate(self.dispatched_runs):
            if existing.target == run.target and existing.run_id == run.run_id:
                self.dispatched_runs[i] = run
                self.touch()
                return
        self.dispatched_runs.append(run)
        self.touch()

    def get_run(self, target: str) -> DispatchedRun | None:
        """Return the most recent run for `target`, or None."""
        matches = [r for r in self.dispatched_runs if r.target == target]
        if not matches:
            return None
        return max(matches, key=lambda r: r.updated_at)

    def update_evidence(self, target: str, status: str) -> None:
        """Record a target's evidence status in the snapshot."""
        self.evidence_snapshot[target] = status
        self.touch()

    def is_sha_drift(self, current_sha: str) -> bool:
        """True if `current_sha` differs from the recorded head_sha."""
        return current_sha != self.head_sha

    def to_dict(self) -> dict[str, Any]:
        return {
            "schema_version": self.schema_version,
            "pr": self.pr,
            "repo": self.repo,
            "branch": self.branch,
            "base_branch": self.base_branch,
            "head_sha": self.head_sha,
            "policy_signature": self.policy_signature,
            "pr_url": self.pr_url,
            "pr_title": self.pr_title,
            "commit_subject": self.commit_subject,
            "dispatched_runs": [r.to_dict() for r in self.dispatched_runs],
            "evidence_snapshot": dict(self.evidence_snapshot),
            "attempt": self.attempt,
            "created_at": self.created_at.isoformat(),
            "updated_at": self.updated_at.isoformat(),
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> ShipState:
        return cls(
            pr=int(d["pr"]),
            repo=d["repo"],
            branch=d["branch"],
            base_branch=d["base_branch"],
            head_sha=d["head_sha"],
            policy_signature=d.get("policy_signature", ""),
            pr_url=d.get("pr_url", ""),
            pr_title=d.get("pr_title", ""),
            commit_subject=d.get("commit_subject", ""),
            dispatched_runs=[
                DispatchedRun.from_dict(r)
                for r in d.get("dispatched_runs", [])
            ],
            evidence_snapshot=dict(d.get("evidence_snapshot", {})),
            attempt=int(d.get("attempt", 1)),
            created_at=datetime.fromisoformat(d["created_at"]),
            updated_at=datetime.fromisoformat(d["updated_at"]),
            schema_version=int(d.get("schema_version", SCHEMA_VERSION)),
        )


@dataclass(frozen=True)
class PruneReport:
    """Summary of what `ShipStateStore.prune()` removed."""

    deleted_active: list[int] = field(default_factory=list)
    deleted_archived: list[str] = field(default_factory=list)

    @property
    def total(self) -> int:
        return len(self.deleted_active) + len(self.deleted_archived)

    def to_dict(self) -> dict[str, Any]:
        return {
            "deleted_active": list(self.deleted_active),
            "deleted_archived": list(self.deleted_archived),
            "total": self.total,
        }


@dataclass
class ShipStateStore:
    """Persists in-flight ship state, one file per PR.

    Active state lives at `<path>/<pr>.json`. On archive, the file
    is moved to `<path>/archive/<pr>-<timestamp>.json`. Writes are
    atomic (tempfile + os.replace) to prevent torn files on crash.
    Reads tolerate missing or corrupt files by returning None so the
    caller can treat a damaged state as "no state" and re-dispatch.
    """

    path: Path

    def __post_init__(self) -> None:
        self.path.mkdir(parents=True, exist_ok=True)
        self._archive_dir.mkdir(parents=True, exist_ok=True)

    @property
    def _archive_dir(self) -> Path:
        return self.path / "archive"

    def _state_path(self, pr: int) -> Path:
        return self.path / f"{pr}.json"

    def get(self, pr: int) -> ShipState | None:
        state_path = self._state_path(pr)
        if not state_path.exists():
            return None
        try:
            data = json.loads(state_path.read_text())
            return ShipState.from_dict(data)
        except (json.JSONDecodeError, KeyError, ValueError):
            # Corrupt file — treat as absent. A fresh ship will
            # overwrite it.
            return None

    def save(self, state: ShipState) -> None:
        """Write state atomically.

        The caller is responsible for updating `state.updated_at`
        (the mutation helpers `upsert_run`, `update_evidence`, and
        `touch` do this automatically). save() writes what it is
        given so tests and audit tools can back-date state files
        when needed.
        """
        state_path = self._state_path(state.pr)
        payload = json.dumps(state.to_dict(), indent=2) + "\n"
        # Atomic write: tempfile in the same directory (so rename is
        # atomic on the same filesystem), then os.replace to swing
        # the final name. Prevents half-written files on crash or
        # concurrent writers.
        fd, tmp_name = tempfile.mkstemp(
            prefix=f".{state.pr}.", suffix=".tmp", dir=str(self.path)
        )
        try:
            with os.fdopen(fd, "w") as f:
                f.write(payload)
            os.replace(tmp_name, state_path)
        except Exception:
            with contextlib.suppress(OSError):
                os.unlink(tmp_name)
            raise

    def delete(self, pr: int) -> None:
        """Remove the active state file for a PR, no archive."""
        state_path = self._state_path(pr)
        if state_path.exists():
            state_path.unlink()

    def archive(self, pr: int) -> Path | None:
        """Move active state to the archive dir. Returns the archived path.

        Returns None if no active state exists for this PR. Archive
        filenames include a UTC timestamp so repeated archives for
        the same PR do not collide (e.g., a PR shipped twice).
        """
        state_path = self._state_path(pr)
        if not state_path.exists():
            return None
        stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        dest = self._archive_dir / f"{pr}-{stamp}.json"
        os.replace(state_path, dest)
        return dest

    def list_active(self) -> list[ShipState]:
        """Return every active (non-archived) ShipState, sorted by PR."""
        states: list[ShipState] = []
        for entry in sorted(self.path.glob("*.json")):
            # pr filenames are pure integers; skip anything else
            # (e.g., stray temp files from an interrupted write).
            if not entry.stem.isdigit():
                continue
            state = self.get(int(entry.stem))
            if state is not None:
                states.append(state)
        return sorted(states, key=lambda s: s.pr)

    def list_archived(self) -> list[Path]:
        """Return every archived file, sorted by name (PR + timestamp)."""
        if not self._archive_dir.exists():
            return []
        return sorted(self._archive_dir.glob("*.json"))

    def prune(
        self,
        *,
        active_days: int = 14,
        archive_days: int = 30,
        closed_prs: set[int] | None = None,
        now: datetime | None = None,
    ) -> PruneReport:
        """Remove stale active and archived state files.

        Rules:
        - An **active** state file is deleted if its `updated_at` is
          older than `active_days` AND (closed_prs is provided and
          the PR is in it). Without a `closed_prs` set, active files
          are never auto-deleted — they may still be in flight.
        - An **archived** file is deleted if its filesystem mtime is
          older than `archive_days`. Archives are tombstones; once
          aged out they have no value.

        Returns a PruneReport listing what was deleted.
        """
        now = now or datetime.now(timezone.utc)
        active_cutoff = now - timedelta(days=active_days)
        archive_cutoff = now - timedelta(days=archive_days)

        deleted_active: list[int] = []
        deleted_archived: list[str] = []

        if closed_prs is not None:
            for state in self.list_active():
                if state.pr not in closed_prs:
                    continue
                if state.updated_at <= active_cutoff:
                    self.delete(state.pr)
                    deleted_active.append(state.pr)

        for archive_path in self.list_archived():
            mtime = datetime.fromtimestamp(
                archive_path.stat().st_mtime, tz=timezone.utc
            )
            if mtime <= archive_cutoff:
                archive_path.unlink()
                deleted_archived.append(archive_path.name)

        return PruneReport(
            deleted_active=deleted_active,
            deleted_archived=deleted_archived,
        )

    def archive_and_replace(
        self, state: ShipState, new_attempt: int | None = None
    ) -> ShipState:
        """Archive the existing state for this PR and start a new attempt.

        Used when a ship restarts for the same PR (e.g., user force-
        pushed and re-ran ship). Returns a fresh ShipState with the
        attempt counter incremented.
        """
        self.archive(state.pr)
        return replace(
            state,
            attempt=(new_attempt if new_attempt is not None else state.attempt + 1),
            dispatched_runs=[],
            evidence_snapshot={},
            created_at=datetime.now(timezone.utc),
            updated_at=datetime.now(timezone.utc),
        )


def compute_policy_signature(
    required_platforms: list[str], target_names: list[str], mode: str
) -> str:
    """Compute a stable digest of the merge policy inputs.

    If the user changes `.shipyard/config.toml` between ship start
    and resume — e.g. adds a new required platform — the signature
    no longer matches and the ship must refuse to resume. This
    prevents silently merging under a different policy than was in
    effect at dispatch time.
    """
    h = hashlib.sha256()
    h.update(b"platforms:")
    for p in sorted(required_platforms):
        h.update(p.encode("utf-8"))
        h.update(b"\x00")
    h.update(b"targets:")
    for t in sorted(target_names):
        h.update(t.encode("utf-8"))
        h.update(b"\x00")
    h.update(b"mode:")
    h.update(mode.encode("utf-8"))
    return h.hexdigest()[:16]

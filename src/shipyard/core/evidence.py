"""Evidence model — per-target SHA proof tracking.

Evidence is the core of Shipyard's merge gate. It records what SHA was
validated, on what machine, by what backend, and when.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from datetime import datetime
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from pathlib import Path


@dataclass(frozen=True)
class EvidenceRecord:
    """Proof that a specific SHA was validated on a specific target."""

    sha: str
    branch: str
    target_name: str
    platform: str
    status: str  # "pass" or "fail"
    backend: str  # "local", "ssh", "namespace-failover", etc.
    completed_at: datetime
    duration_secs: float | None = None
    host: str | None = None
    # Failover provenance
    primary_backend: str | None = None
    failover_reason: str | None = None
    provider: str | None = None
    runner_profile: str | None = None
    # Coarse-grained failure taxonomy when ``status == "fail"``. One of
    # "INFRA" | "TIMEOUT" | "CONTRACT" | "TEST" | "UNKNOWN". None on a
    # passing record. See ``shipyard.core.classify``.
    failure_class: str | None = None
    # Cross-PR evidence reuse provenance. When set, this record was
    # synthesized from an earlier PASS on an ancestor SHA because the
    # diff between that SHA and HEAD did not touch any path the target
    # cares about (see ``reuse_if_paths_unchanged`` on target config).
    # Value is the ancestor SHA the evidence was borrowed from.
    reused_from: str | None = None
    # Fingerprints of the validation contract + stages in effect when
    # the record was written. Reuse compares these between the
    # candidate ancestor record and the current target config and
    # refuses reuse on drift (e.g., contract markers changed, a new
    # build stage was added). ``None`` on pre-reuse records.
    contract_digest: str | None = None
    stages_signature: str | None = None

    @property
    def passed(self) -> bool:
        return self.status == "pass"

    @property
    def reused(self) -> bool:
        """True if this record was borrowed from an ancestor PASS."""
        return self.reused_from is not None

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "sha": self.sha,
            "branch": self.branch,
            "target": self.target_name,
            "platform": self.platform,
            "status": self.status,
            "backend": self.backend,
            "completed_at": self.completed_at.isoformat(),
        }
        for key in (
            "duration_secs", "host", "primary_backend", "failover_reason",
            "provider", "runner_profile", "failure_class",
            "reused_from", "contract_digest", "stages_signature",
        ):
            val = getattr(self, key)
            if val is not None:
                d[key] = val
        return d

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> EvidenceRecord:
        return cls(
            sha=d["sha"],
            branch=d["branch"],
            target_name=d["target"],
            platform=d["platform"],
            status=d["status"],
            backend=d["backend"],
            completed_at=datetime.fromisoformat(d["completed_at"]),
            duration_secs=d.get("duration_secs"),
            host=d.get("host"),
            primary_backend=d.get("primary_backend"),
            failover_reason=d.get("failover_reason"),
            provider=d.get("provider"),
            runner_profile=d.get("runner_profile"),
            failure_class=d.get("failure_class"),
            reused_from=d.get("reused_from"),
            contract_digest=d.get("contract_digest"),
            stages_signature=d.get("stages_signature"),
        )


@dataclass
class EvidenceStore:
    """Persists and queries evidence records.

    Evidence is stored per-branch, keyed by target name. Only the most
    recent record per target is kept (you only need to know the latest
    proof for merge gating).
    """

    path: Path
    _cache: dict[str, dict[str, EvidenceRecord]] = field(default_factory=dict, repr=False)

    def __post_init__(self) -> None:
        self.path.mkdir(parents=True, exist_ok=True)

    def record(self, evidence: EvidenceRecord) -> None:
        """Store an evidence record, replacing any existing for same branch+target."""
        branch_key = _sanitize_branch(evidence.branch)
        branch_data = self._load_branch(branch_key)
        branch_data[evidence.target_name] = evidence
        self._save_branch(branch_key, branch_data)

    def get_branch(self, branch: str) -> dict[str, EvidenceRecord]:
        """Get all evidence for a branch, keyed by target name."""
        return dict(self._load_branch(_sanitize_branch(branch)))

    def get_target(self, branch: str, target_name: str) -> EvidenceRecord | None:
        """Get evidence for a specific branch + target."""
        return self._load_branch(_sanitize_branch(branch)).get(target_name)

    def is_merge_ready(
        self,
        branch: str,
        sha: str,
        required_platforms: list[str],
    ) -> tuple[bool, dict[str, EvidenceRecord | None]]:
        """Check if all required platforms have passing evidence for this SHA.

        Returns (ready, evidence_map) where evidence_map has an entry for
        each required platform (None if no evidence exists).
        """
        records = self.get_branch(branch)
        evidence_map: dict[str, EvidenceRecord | None] = {}
        all_green = True

        for platform in required_platforms:
            # Find evidence for this platform
            match = None
            for rec in records.values():
                if rec.platform == platform and rec.sha == sha and rec.passed:
                    match = rec
                    break
            evidence_map[platform] = match
            if match is None:
                all_green = False

        return all_green, evidence_map

    def query_passing_for_target(
        self,
        target_name: str,
        sha_candidates: list[str],
    ) -> EvidenceRecord | None:
        """Find the most recent PASS record for ``target_name`` whose
        SHA is in ``sha_candidates``.

        Searches across every branch file in the store. This is the
        primary lookup for cross-PR evidence reuse: pass in the list
        of ancestor SHAs reachable from HEAD (newest first) and this
        returns the first PASS that matches. Returns None if no
        ancestor has passing evidence for the target.

        ``sha_candidates`` is treated as an ordered preference list —
        earlier entries win on ties. Records with ``reused_from`` set
        are **excluded** (we only borrow from real runs, never
        chain-reuse from another reuse).
        """
        candidate_set = {sha: rank for rank, sha in enumerate(sha_candidates)}
        best: tuple[int, EvidenceRecord] | None = None
        for entry in self.path.glob("*.json"):
            branch_key = entry.stem
            try:
                records = self._load_branch(branch_key)
            except (OSError, ValueError):
                continue
            for rec in records.values():
                if rec.target_name != target_name or not rec.passed:
                    continue
                if rec.reused_from is not None:
                    continue
                rank = candidate_set.get(rec.sha)
                if rank is None:
                    continue
                if best is None or rank < best[0]:
                    best = (rank, rec)
        return best[1] if best else None

    def _branch_file(self, branch_key: str) -> Path:
        return self.path / f"{branch_key}.json"

    def _load_branch(self, branch_key: str) -> dict[str, EvidenceRecord]:
        if branch_key in self._cache:
            return self._cache[branch_key]

        path = self._branch_file(branch_key)
        if not path.exists():
            self._cache[branch_key] = {}
            return self._cache[branch_key]

        data = json.loads(path.read_text())
        records = {k: EvidenceRecord.from_dict(v) for k, v in data.items()}
        self._cache[branch_key] = records
        return records

    def _save_branch(self, branch_key: str, records: dict[str, EvidenceRecord]) -> None:
        self._cache[branch_key] = records
        path = self._branch_file(branch_key)
        data = {k: v.to_dict() for k, v in records.items()}
        path.write_text(json.dumps(data, indent=2) + "\n")


def _sanitize_branch(branch: str) -> str:
    """Convert branch name to a safe filename."""
    return branch.replace("/", "--").replace("\\", "--")

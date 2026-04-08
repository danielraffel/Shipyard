"""Merge-on-green logic.

Orchestrates the full ship flow: push, create PR, validate on all
required platforms, and merge only when evidence proves all green.
"""

from __future__ import annotations

import subprocess
from dataclasses import dataclass
from typing import Any

from shipyard.core.config import Config
from shipyard.core.evidence import EvidenceStore
from shipyard.core.queue import Queue
from shipyard.ship.pr import GhError, PrInfo, create_pr, find_pr_for_branch, merge_pr


@dataclass(frozen=True)
class MergeCheck:
    """Result of checking whether a branch is ready to merge."""

    ready: bool
    sha: str
    branch: str
    required_platforms: list[str]
    passing: list[str]
    missing: list[str]
    failing: list[str]

    def to_dict(self) -> dict[str, Any]:
        return {
            "ready": self.ready,
            "sha": self.sha,
            "branch": self.branch,
            "required_platforms": self.required_platforms,
            "passing": self.passing,
            "missing": self.missing,
            "failing": self.failing,
        }


def can_merge(
    evidence_store: EvidenceStore,
    branch: str,
    sha: str,
    required_platforms: list[str],
) -> MergeCheck:
    """Check if all required platforms have passing evidence for this SHA.

    Returns a MergeCheck with detailed status per platform.
    """
    passing: list[str] = []
    missing: list[str] = []
    failing: list[str] = []

    records = evidence_store.get_branch(branch)

    for platform in required_platforms:
        found = False
        for rec in records.values():
            if rec.platform == platform and rec.sha == sha:
                found = True
                if rec.passed:
                    passing.append(platform)
                else:
                    failing.append(platform)
                break
        if not found:
            missing.append(platform)

    ready = len(passing) == len(required_platforms)

    return MergeCheck(
        ready=ready,
        sha=sha,
        branch=branch,
        required_platforms=required_platforms,
        passing=passing,
        missing=missing,
        failing=failing,
    )


@dataclass(frozen=True)
class ShipResult:
    """Outcome of a full ship flow."""

    success: bool
    pr: PrInfo | None = None
    merge_check: MergeCheck | None = None
    error: str | None = None
    job_id: str | None = None

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"success": self.success}
        if self.pr:
            d["pr"] = self.pr.to_dict()
        if self.merge_check:
            d["merge_check"] = self.merge_check.to_dict()
        if self.error:
            d["error"] = self.error
        if self.job_id:
            d["job_id"] = self.job_id
        return d


def ship(
    config: Config,
    queue: Queue,
    evidence_store: EvidenceStore,
) -> ShipResult:
    """Full ship flow: push, PR, validate, merge on green.

    Steps:
    1. Get current branch and SHA
    2. Push to remote
    3. Create or find existing PR
    4. Run validation via the queue
    5. Check evidence — merge if all green
    """
    # Get git state
    sha = _git_sha()
    branch = _git_branch()
    if not sha or not branch:
        return ShipResult(success=False, error="Not in a git repository")

    if branch == "main":
        return ShipResult(success=False, error="Cannot ship from main — create a branch first")

    base = config.get("ship.base_branch", "main")
    required_platforms = config.merge_require_platforms

    if not required_platforms:
        return ShipResult(
            success=False,
            error="No required platforms configured in merge.require_platforms",
        )

    # Find or create PR
    try:
        pr = find_pr_for_branch(branch)
        if pr is None:
            title = _default_pr_title(branch)
            body = f"Ship {branch} @ {sha[:8]}\n\nAutomated by Shipyard."
            pr = create_pr(branch, base, title, body)
    except GhError as e:
        return ShipResult(success=False, error=f"PR creation failed: {e}")

    # Run validation
    from shipyard.core.job import Job, ValidationMode

    job = Job.create(
        sha=sha,
        branch=branch,
        target_names=list(config.targets.keys()),
        mode=ValidationMode.FULL,
    )
    job = queue.enqueue(job)

    # Check if we already have enough evidence to merge
    check = can_merge(evidence_store, branch, sha, required_platforms)

    if check.ready:
        try:
            merge_pr(pr.number)
            return ShipResult(
                success=True,
                pr=pr,
                merge_check=check,
                job_id=job.id,
            )
        except GhError as e:
            return ShipResult(
                success=False,
                pr=pr,
                merge_check=check,
                error=f"Merge failed: {e}",
                job_id=job.id,
            )

    # Not ready yet — return current state
    return ShipResult(
        success=False,
        pr=pr,
        merge_check=check,
        error="Not all platforms green yet — run validation first",
        job_id=job.id,
    )


def _git_sha() -> str | None:
    try:
        return subprocess.check_output(
            ["git", "rev-parse", "HEAD"], text=True, stderr=subprocess.DEVNULL
        ).strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None


def _git_branch() -> str | None:
    try:
        return subprocess.check_output(
            ["git", "rev-parse", "--abbrev-ref", "HEAD"],
            text=True,
            stderr=subprocess.DEVNULL,
        ).strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None


def _default_pr_title(branch: str) -> str:
    """Generate a PR title from a branch name."""
    # feature/foo-bar -> Foo bar
    name = branch.split("/")[-1]
    name = name.replace("-", " ").replace("_", " ")
    return name.capitalize()

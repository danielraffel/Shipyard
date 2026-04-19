"""Merge-on-green logic.

Orchestrates the full ship flow: push, create PR, validate on all
required platforms, and merge only when evidence proves all green.
"""

from __future__ import annotations

import subprocess
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

from shipyard.core.quarantine import QuarantineList, is_advisory_failure
from shipyard.ship.pr import GhError, PrInfo, create_pr, find_pr_for_branch, merge_pr

if TYPE_CHECKING:
    from shipyard.core.config import Config
    from shipyard.core.evidence import EvidenceStore
    from shipyard.core.queue import Queue


@dataclass(frozen=True)
class MergeCheck:
    """Result of checking whether a branch is ready to merge.

    Quarantine interaction: targets listed in ``.shipyard/quarantine.toml``
    whose failure class is TEST or UNKNOWN don't count against ``ready``
    — they land in ``advisory`` instead of ``failing`` so the reviewer
    still sees them. Failures with class INFRA / TIMEOUT / CONTRACT are
    never suppressed by quarantine (they indicate real, fixable
    infrastructure or contract problems).
    """

    ready: bool
    sha: str
    branch: str
    required_platforms: list[str]
    passing: list[str]
    missing: list[str]
    failing: list[str]
    advisory: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "ready": self.ready,
            "sha": self.sha,
            "branch": self.branch,
            "required_platforms": self.required_platforms,
            "passing": self.passing,
            "missing": self.missing,
            "failing": self.failing,
        }
        if self.advisory:
            d["advisory"] = self.advisory
        return d


def can_merge(
    evidence_store: EvidenceStore,
    branch: str,
    sha: str,
    required_platforms: list[str],
    quarantine: QuarantineList | None = None,
) -> MergeCheck:
    """Check if all required platforms have passing evidence for this SHA.

    When ``quarantine`` is supplied, targets whose evidence shows a
    suppressible failure class (TEST / UNKNOWN) AND whose *target name*
    appears on the list are moved from ``failing`` to ``advisory`` and
    do not block ``ready``. When ``quarantine`` is None, behavior is
    identical to the pre-quarantine code path (backward compatible).
    """
    passing: list[str] = []
    missing: list[str] = []
    failing: list[str] = []
    advisory: list[str] = []

    records = evidence_store.get_branch(branch)
    q = quarantine or QuarantineList(entries=[], path=None)

    for platform in required_platforms:
        found = False
        for rec in records.values():
            if rec.platform == platform and rec.sha == sha:
                found = True
                if rec.passed:
                    passing.append(platform)
                elif is_advisory_failure(q, rec.target_name, rec.failure_class):
                    advisory.append(platform)
                else:
                    failing.append(platform)
                break
        if not found:
            missing.append(platform)

    # A platform counts toward merge-ready if it's passing or advisory.
    ready = (len(passing) + len(advisory)) == len(required_platforms) and not failing

    return MergeCheck(
        ready=ready,
        sha=sha,
        branch=branch,
        required_platforms=required_platforms,
        passing=passing,
        missing=missing,
        failing=failing,
        advisory=advisory,
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

    .. note::
       This function is **not** the implementation behind the
       ``shipyard ship`` CLI command. ``cli.py:ship`` is the
       authoritative ship orchestrator and supports more options
       (``--base``, ``--allow-root-mismatch``, ``--auto-create-base``,
       ``--resume-from``, JSON output, etc.). This function is kept
       as a programmatic API surface for callers that import
       ``shipyard.ship.merge`` directly; it intentionally does the
       same logical thing in a simpler shape.

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

    # Check if we already have enough evidence to merge, honoring
    # any `.shipyard/quarantine.toml` entries.
    quarantine = QuarantineList.load_from_project(config.project_dir)
    check = can_merge(
        evidence_store, branch, sha, required_platforms,
        quarantine=quarantine,
    )

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

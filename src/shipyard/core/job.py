"""Job and run domain types.

These are the core data structures that flow through every part of Shipyard.
Jobs are immutable once created — state transitions produce new instances.
"""

from __future__ import annotations

import uuid
from dataclasses import dataclass, field, replace
from datetime import datetime, timezone
from enum import Enum
from typing import Any


class Priority(Enum):
    """Job scheduling priority."""

    LOW = 10
    NORMAL = 50
    HIGH = 100


class ValidationMode(Enum):
    """How thorough the validation should be."""

    FULL = "full"
    SMOKE = "smoke"


class JobStatus(Enum):
    """Lifecycle state of a job."""

    PENDING = "pending"
    RUNNING = "running"
    COMPLETED = "completed"
    CANCELLED = "cancelled"


class TargetStatus(Enum):
    """Result state of a single target within a run."""

    PENDING = "pending"
    RUNNING = "running"
    PASS = "pass"
    FAIL = "fail"
    ERROR = "error"
    UNREACHABLE = "unreachable"
    CANCELLED = "cancelled"


@dataclass(frozen=True)
class TargetResult:
    """Outcome of validating one target in a run."""

    target_name: str
    platform: str
    status: TargetStatus
    backend: str  # "local", "ssh", "namespace-failover", "github-hosted", etc.
    duration_secs: float | None = None
    started_at: datetime | None = None
    completed_at: datetime | None = None
    log_path: str | None = None
    phase: str | None = None
    last_output_at: datetime | None = None
    last_heartbeat_at: datetime | None = None
    quiet_for_secs: float | None = None
    liveness: str | None = None
    # Failover provenance
    primary_backend: str | None = None
    failover_reason: str | None = None
    provider: str | None = None
    runner_profile: str | None = None
    # Error detail
    error_message: str | None = None
    # Validation contract violation. When the project declares
    # required contract markers in `[validation.contract]`, the
    # executor records which markers were seen and which were missing.
    # If `enforce = true` and any required marker is missing, the
    # status is forced to FAIL and `contract_violation` carries a
    # human-readable explanation. When `enforce = false`, missing
    # markers are recorded here as a warning but the status is
    # unchanged.
    contract_markers_seen: tuple[str, ...] = ()
    contract_markers_missing: tuple[str, ...] = ()
    contract_violation: str | None = None
    # Coarse-grained failure taxonomy. Populated by the executor
    # (or failover chain) whenever ``status`` is FAIL / ERROR /
    # UNREACHABLE. Values: "INFRA" | "TIMEOUT" | "CONTRACT" | "TEST"
    # | "UNKNOWN". None for PASS/PENDING/RUNNING results. See
    # ``shipyard.core.classify`` for heuristics.
    failure_class: str | None = None

    @property
    def passed(self) -> bool:
        return self.status == TargetStatus.PASS

    def with_updates(self, **kwargs: Any) -> TargetResult:
        """Return a copy with the provided fields updated."""
        return replace(self, **kwargs)

    @property
    def is_terminal(self) -> bool:
        """Whether this result is final (no more state changes expected)."""
        return self.status in (
            TargetStatus.PASS,
            TargetStatus.FAIL,
            TargetStatus.ERROR,
            TargetStatus.UNREACHABLE,
            TargetStatus.CANCELLED,
        )

    def to_dict(self) -> dict[str, Any]:
        """Serialize for JSON output."""
        d: dict[str, Any] = {
            "target": self.target_name,
            "platform": self.platform,
            "status": self.status.value,
            "backend": self.backend,
        }
        if self.duration_secs is not None:
            d["duration_secs"] = round(self.duration_secs, 1)
        if self.started_at:
            d["started_at"] = self.started_at.isoformat()
        if self.completed_at:
            d["completed_at"] = self.completed_at.isoformat()
        if self.log_path:
            d["log_path"] = self.log_path
        if self.phase:
            d["phase"] = self.phase
        if self.last_output_at:
            d["last_output_at"] = self.last_output_at.isoformat()
        if self.last_heartbeat_at:
            d["last_heartbeat_at"] = self.last_heartbeat_at.isoformat()
        if self.quiet_for_secs is not None:
            d["quiet_for_secs"] = round(self.quiet_for_secs, 1)
        if self.liveness:
            d["liveness"] = self.liveness
        if self.primary_backend:
            d["primary_backend"] = self.primary_backend
        if self.failover_reason:
            d["failover_reason"] = self.failover_reason
        if self.provider:
            d["provider"] = self.provider
        if self.runner_profile:
            d["runner_profile"] = self.runner_profile
        if self.error_message:
            d["error_message"] = self.error_message
        if self.contract_markers_seen:
            d["contract_markers_seen"] = list(self.contract_markers_seen)
        if self.contract_markers_missing:
            d["contract_markers_missing"] = list(self.contract_markers_missing)
        if self.contract_violation:
            d["contract_violation"] = self.contract_violation
        if self.failure_class:
            d["failure_class"] = self.failure_class
        return d


def _generate_id() -> str:
    """Short, human-readable job ID."""
    now = datetime.now(timezone.utc)
    short = uuid.uuid4().hex[:6]
    return f"sy-{now.strftime('%Y%m%d')}-{short}"


@dataclass(frozen=True)
class Job:
    """A validation run across one or more targets for a specific SHA.

    Jobs are immutable. State transitions (pending → running → completed)
    produce new Job instances via the transition methods below.
    """

    id: str
    sha: str
    branch: str
    mode: ValidationMode
    target_names: tuple[str, ...]
    priority: Priority = Priority.NORMAL
    status: JobStatus = JobStatus.PENDING
    created_at: datetime = field(default_factory=lambda: datetime.now(timezone.utc))
    started_at: datetime | None = None
    completed_at: datetime | None = None
    results: dict[str, TargetResult] = field(default_factory=dict)

    @classmethod
    def create(
        cls,
        sha: str,
        branch: str,
        target_names: list[str],
        mode: ValidationMode = ValidationMode.FULL,
        priority: Priority = Priority.NORMAL,
    ) -> Job:
        """Create a new pending job."""
        return cls(
            id=_generate_id(),
            sha=sha,
            branch=branch,
            mode=mode,
            target_names=tuple(target_names),
            priority=priority,
        )

    def start(self) -> Job:
        """Transition from PENDING to RUNNING."""
        if self.status != JobStatus.PENDING:
            raise ValueError(f"Cannot start job in state {self.status.value}")
        return Job(
            id=self.id,
            sha=self.sha,
            branch=self.branch,
            mode=self.mode,
            target_names=self.target_names,
            priority=self.priority,
            status=JobStatus.RUNNING,
            created_at=self.created_at,
            started_at=datetime.now(timezone.utc),
            results=dict(self.results),
        )

    def complete(self) -> Job:
        """Transition from RUNNING to COMPLETED."""
        if self.status != JobStatus.RUNNING:
            raise ValueError(f"Cannot complete job in state {self.status.value}")
        return Job(
            id=self.id,
            sha=self.sha,
            branch=self.branch,
            mode=self.mode,
            target_names=self.target_names,
            priority=self.priority,
            status=JobStatus.COMPLETED,
            created_at=self.created_at,
            started_at=self.started_at,
            completed_at=datetime.now(timezone.utc),
            results=dict(self.results),
        )

    def cancel(self) -> Job:
        """Cancel the job from any non-terminal state."""
        if self.status in (JobStatus.COMPLETED, JobStatus.CANCELLED):
            raise ValueError(f"Cannot cancel job in state {self.status.value}")
        return Job(
            id=self.id,
            sha=self.sha,
            branch=self.branch,
            mode=self.mode,
            target_names=self.target_names,
            priority=self.priority,
            status=JobStatus.CANCELLED,
            created_at=self.created_at,
            started_at=self.started_at,
            completed_at=datetime.now(timezone.utc),
            results=dict(self.results),
        )

    def with_priority(self, priority: Priority) -> Job:
        """Return a new job with a different priority."""
        return Job(
            id=self.id,
            sha=self.sha,
            branch=self.branch,
            mode=self.mode,
            target_names=self.target_names,
            priority=priority,
            status=self.status,
            created_at=self.created_at,
            started_at=self.started_at,
            completed_at=self.completed_at,
            results=dict(self.results),
        )

    def with_result(self, result: TargetResult) -> Job:
        """Return a new job with an updated target result."""
        new_results = dict(self.results)
        new_results[result.target_name] = result
        return Job(
            id=self.id,
            sha=self.sha,
            branch=self.branch,
            mode=self.mode,
            target_names=self.target_names,
            priority=self.priority,
            status=self.status,
            created_at=self.created_at,
            started_at=self.started_at,
            completed_at=self.completed_at,
            results=new_results,
        )

    @property
    def passed(self) -> bool:
        """All targets passed."""
        return (
            self.status == JobStatus.COMPLETED
            and len(self.results) == len(self.target_names)
            and all(r.passed for r in self.results.values())
        )

    @property
    def all_targets_terminal(self) -> bool:
        """Whether every target has a final result."""
        return len(self.results) == len(self.target_names) and all(
            r.is_terminal for r in self.results.values()
        )

    def to_dict(self) -> dict[str, Any]:
        """Serialize for JSON output."""
        d: dict[str, Any] = {
            "id": self.id,
            "sha": self.sha,
            "branch": self.branch,
            "mode": self.mode.value,
            "targets": list(self.target_names),
            "priority": self.priority.name.lower(),
            "status": self.status.value,
            "overall": "pass" if self.passed else ("fail" if self.status == JobStatus.COMPLETED else self.status.value),
            "created_at": self.created_at.isoformat(),
        }
        if self.started_at:
            d["started_at"] = self.started_at.isoformat()
        if self.completed_at:
            d["completed_at"] = self.completed_at.isoformat()
        if self.results:
            d["results"] = {
                name: result.to_dict() for name, result in self.results.items()
            }
        return d

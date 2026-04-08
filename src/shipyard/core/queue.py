"""Machine-global job queue with file locking.

The queue is shared across all worktrees and agents on one machine.
Only one drain owner can process jobs at a time (file-lock enforced).
Jobs are ordered by priority (high first) then FIFO.
"""

from __future__ import annotations

import fcntl
import json
import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from shipyard.core.job import Job, JobStatus, Priority, ValidationMode

KEEP_COMPLETED = 25


@dataclass
class Queue:
    """Persistent, file-locked job queue."""

    state_dir: Path
    _jobs: list[Job] = field(default_factory=list, repr=False)
    _loaded: bool = field(default=False, repr=False)

    def __post_init__(self) -> None:
        self.state_dir.mkdir(parents=True, exist_ok=True)

    @property
    def _queue_file(self) -> Path:
        return self.state_dir / "queue.json"

    @property
    def _lock_file(self) -> Path:
        return self.state_dir / "queue.lock"

    def _ensure_loaded(self) -> None:
        if not self._loaded:
            self._load()

    def _load(self) -> None:
        """Load queue state from disk."""
        if self._queue_file.exists():
            data = json.loads(self._queue_file.read_text())
            self._jobs = [_job_from_dict(d) for d in data.get("jobs", [])]
        else:
            self._jobs = []
        self._loaded = True

    def _save(self) -> None:
        """Write queue state to disk."""
        data = {"jobs": [_job_to_dict(j) for j in self._jobs]}
        self._queue_file.write_text(json.dumps(data, indent=2) + "\n")

    def enqueue(self, job: Job) -> Job:
        """Add a job to the queue. Returns the job (with ID assigned).

        If a pending job exists for the same branch, it is superseded
        (replaced) by the new one — but running jobs are never cancelled.
        """
        self._ensure_loaded()

        # Supersede pending jobs for the same branch + target set + mode.
        # A narrower rerun (different targets or mode) is NOT superseded.
        self._jobs = [
            j
            for j in self._jobs
            if not (
                j.branch == job.branch
                and j.status == JobStatus.PENDING
                and j.target_names == job.target_names
                and j.mode == job.mode
            )
        ]

        self._jobs.append(job)
        self._save()
        return job

    def next_pending(self) -> Job | None:
        """Return the highest-priority pending job, or None."""
        self._ensure_loaded()
        pending = [j for j in self._jobs if j.status == JobStatus.PENDING]
        if not pending:
            return None
        # Sort by priority (high first), then by creation time (FIFO)
        pending.sort(key=lambda j: (-j.priority.value, j.created_at))
        return pending[0]

    def update(self, job: Job) -> None:
        """Replace a job in the queue (matched by ID)."""
        self._ensure_loaded()
        self._jobs = [job if j.id == job.id else j for j in self._jobs]
        self._trim_completed()
        self._save()

    def get(self, job_id: str) -> Job | None:
        """Look up a job by ID."""
        self._ensure_loaded()
        for j in self._jobs:
            if j.id == job_id:
                return j
        return None

    def get_active(self) -> Job | None:
        """Return the currently running job, if any."""
        self._ensure_loaded()
        for j in self._jobs:
            if j.status == JobStatus.RUNNING:
                return j
        return None

    def get_recent(self, limit: int = 10) -> list[Job]:
        """Return recently completed jobs, newest first."""
        self._ensure_loaded()
        completed = [j for j in self._jobs if j.status == JobStatus.COMPLETED]
        completed.sort(key=lambda j: j.completed_at or j.created_at, reverse=True)
        return completed[:limit]

    @property
    def pending_count(self) -> int:
        self._ensure_loaded()
        return sum(1 for j in self._jobs if j.status == JobStatus.PENDING)

    @property
    def running_count(self) -> int:
        self._ensure_loaded()
        return sum(1 for j in self._jobs if j.status == JobStatus.RUNNING)

    def _trim_completed(self) -> None:
        """Keep only the most recent KEEP_COMPLETED completed jobs."""
        completed = [j for j in self._jobs if j.status == JobStatus.COMPLETED]
        non_completed = [j for j in self._jobs if j.status != JobStatus.COMPLETED]

        completed.sort(key=lambda j: j.completed_at or j.created_at, reverse=True)
        kept = completed[:KEEP_COMPLETED]

        self._jobs = non_completed + kept

    def acquire_drain_lock(self) -> _DrainLock | None:
        """Try to acquire exclusive drain ownership.

        Returns a DrainLock context manager on success, None if another
        process holds the lock.
        """
        lock = _DrainLock(self._lock_file)
        if lock.acquire():
            return lock
        return None


class _DrainLock:
    """File-based exclusive lock for drain ownership."""

    def __init__(self, path: Path) -> None:
        self._path = path
        self._fd: int | None = None

    def acquire(self) -> bool:
        self._path.parent.mkdir(parents=True, exist_ok=True)
        self._fd = os.open(str(self._path), os.O_CREAT | os.O_RDWR)
        try:
            fcntl.flock(self._fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            # Write our PID for debugging
            os.ftruncate(self._fd, 0)
            os.write(self._fd, f"{os.getpid()}\n".encode())
            return True
        except OSError:
            os.close(self._fd)
            self._fd = None
            return False

    def release(self) -> None:
        if self._fd is not None:
            fcntl.flock(self._fd, fcntl.LOCK_UN)
            os.close(self._fd)
            self._fd = None

    def __enter__(self) -> _DrainLock:
        return self

    def __exit__(self, *args: Any) -> None:
        self.release()


# ---- Serialization ----


def _job_to_dict(job: Job) -> dict[str, Any]:
    return job.to_dict()


def _job_from_dict(d: dict[str, Any]) -> Job:
    from datetime import datetime

    from shipyard.core.job import TargetResult, TargetStatus

    results: dict[str, TargetResult] = {}
    if "results" in d:
        for name, rd in d["results"].items():
            results[name] = TargetResult(
                target_name=rd["target"],
                platform=rd["platform"],
                status=TargetStatus(rd["status"]),
                backend=rd["backend"],
                duration_secs=rd.get("duration_secs"),
                started_at=datetime.fromisoformat(rd["started_at"]) if rd.get("started_at") else None,
                completed_at=datetime.fromisoformat(rd["completed_at"]) if rd.get("completed_at") else None,
                log_path=rd.get("log_path"),
                primary_backend=rd.get("primary_backend"),
                failover_reason=rd.get("failover_reason"),
                provider=rd.get("provider"),
                runner_profile=rd.get("runner_profile"),
                error_message=rd.get("error_message"),
            )

    return Job(
        id=d["id"],
        sha=d["sha"],
        branch=d["branch"],
        mode=ValidationMode(d["mode"]),
        target_names=tuple(d["targets"]),
        priority=Priority[d.get("priority", "normal").upper()],
        status=JobStatus(d["status"]),
        created_at=datetime.fromisoformat(d["created_at"]),
        started_at=datetime.fromisoformat(d["started_at"]) if d.get("started_at") else None,
        completed_at=datetime.fromisoformat(d["completed_at"]) if d.get("completed_at") else None,
        results=results,
    )

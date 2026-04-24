"""Machine-global job queue with file locking.

The queue is shared across all worktrees and agents on one machine.
Only one drain owner can process jobs at a time (file-lock enforced).
Jobs are ordered by priority (high first) then FIFO.
"""

from __future__ import annotations

import contextlib
import json
import os
import sys
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

from shipyard.core.job import Job, JobStatus, Priority, ValidationMode

if TYPE_CHECKING:
    from pathlib import Path

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
        """Load queue state from disk, recovering stale jobs.

        Any job in RUNNING state is checked for a live process. If
        no process is alive for the job (the ship/run command that
        owned it crashed, was killed, or the SSH session dropped),
        the job is marked COMPLETED with an error status. This
        prevents ghost "running" entries from blocking the queue
        after a crash.

        A zero-byte or corrupt queue.json (the failure mode tracked
        by #102, where a kill mid-write truncated the file) is
        treated as "no queue" — we re-initialize empty and the next
        save will write a fresh valid file. Losing the snapshot is
        strictly better than erroring on every subsequent invocation.
        """
        if self._queue_file.exists():
            raw = self._queue_file.read_text()
            if not raw.strip():
                # Zero-byte or whitespace-only file (crashed mid-write
                # on a pre-atomic shipyard, or manual corruption).
                self._jobs = []
            else:
                try:
                    data = json.loads(raw)
                except json.JSONDecodeError:
                    # Partial JSON from a crashed pre-atomic writer.
                    # Atomic writes prevent this going forward, but
                    # old state files should still be recoverable.
                    self._jobs = []
                else:
                    self._jobs = [
                        _job_from_dict(d) for d in data.get("jobs", [])
                    ]
        else:
            self._jobs = []

        # Reconcile stale running jobs — if no process holds the
        # drain lock, any "running" job is orphaned.
        stale_running = [
            j for j in self._jobs if j.status == JobStatus.RUNNING
        ]
        if stale_running and not self._is_drain_active():
            import dataclasses
            from datetime import datetime, timezone

            from shipyard.core.job import TargetResult, TargetStatus

            now = datetime.now(timezone.utc)
            for i, job in enumerate(self._jobs):
                if job.status != JobStatus.RUNNING:
                    continue
                # Build error results for incomplete targets
                new_results = dict(job.results)
                for name in job.target_names:
                    if name not in new_results:
                        new_results[name] = TargetResult(
                            target_name=name,
                            platform="unknown",
                            status=TargetStatus.ERROR,
                            backend="unknown",
                            error_message=(
                                "Process died mid-validation; "
                                "job recovered on startup"
                            ),
                        )
                # Replace the frozen job with an updated copy
                self._jobs[i] = dataclasses.replace(
                    job,
                    status=JobStatus.COMPLETED,
                    completed_at=now,
                    results=new_results,
                )
            self._save()

        self._loaded = True

    def _is_drain_active(self) -> bool:
        """Check if any process holds the drain lock (non-blocking)."""
        if not self._lock_file.exists():
            return False
        try:
            fd = os.open(str(self._lock_file), os.O_RDWR)
            try:
                if sys.platform == "win32":
                    import msvcrt
                    msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
                    msvcrt.locking(fd, msvcrt.LK_UNLCK, 1)
                else:
                    import fcntl
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    fcntl.flock(fd, fcntl.LOCK_UN)
                # Lock acquired + released → nobody holds it → stale
                return False
            except OSError:
                # Lock held by another process → drain is active
                return True
            finally:
                os.close(fd)
        except OSError:
            return False

    def _save(self) -> None:
        """Write queue state to disk atomically.

        The write path is tmp-file + fsync + rename. If the writer
        is killed between the write and the rename, `queue.json` is
        untouched and the in-flight update is lost; without this,
        a kill mid-write produced a zero-byte `queue.json` that
        crashed every subsequent `shipyard` invocation with a
        JSONDecodeError (#102, pulp#528, workaround in pulp#534).

        The tmp file name includes the writing process's PID and a
        random suffix (via `tempfile.mkstemp`) so concurrent writers
        don't step on each other's tmp files. `Path.replace` is
        atomic on POSIX and Windows for same-directory renames, so
        concurrent renames resolve to last-writer-wins without torn
        reads ever being observable.

        Also clears any pre-existing `queue.json.tmp` left behind by
        a crashed pre-atomic writer — that file is from a dead
        process and has no claim on the queue directory.
        """
        import tempfile

        data = {"jobs": [_job_to_dict(j) for j in self._jobs]}
        payload = json.dumps(data, indent=2) + "\n"

        # One-shot sweep of the legacy tmp name (pre-atomic writers
        # wrote to `queue.json.tmp`). We don't sweep pid-suffixed
        # tmp files here because those may belong to a live peer
        # writer; the cleanup command handles aged-out tmp files.
        legacy_tmp = self._queue_file.with_suffix(".json.tmp")
        if legacy_tmp.exists():
            with contextlib.suppress(OSError):
                legacy_tmp.unlink()

        # Per-writer unique tmp so concurrent processes don't collide.
        fd, tmp_name = tempfile.mkstemp(
            prefix=".queue-", suffix=".json.tmp", dir=str(self.state_dir)
        )
        tmp_path = self.state_dir / os.path.basename(tmp_name)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as f:
                f.write(payload)
                f.flush()
                os.fsync(f.fileno())
            # Path.replace is atomic on POSIX and Windows for same-
            # directory renames — this is the step that makes the
            # update visible. Either the old file or the fully-
            # written new file is present; never a torn half.
            #
            # On Windows, MoveFileEx (which backs os.replace) can
            # fail with PermissionError (WinError 5) when another
            # process is mid-rename on the same target or has the
            # target open for reading. This is the normal file-share
            # contention window, not a real failure. A small retry
            # loop covers it. POSIX renames don't hit this, so the
            # first attempt always wins there.
            _retry_replace_on_windows(tmp_path, self._queue_file)
        except Exception:
            # Clean up the tmp file so a failed save doesn't leave
            # stale files behind. The destination is untouched, so
            # the next _load() still sees the previous valid state.
            if tmp_path.exists():
                with contextlib.suppress(OSError):
                    tmp_path.unlink()
            raise

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

    def get_pending(self) -> list[Job]:
        """Return pending jobs, sorted by priority (high first) then FIFO."""
        self._ensure_loaded()
        pending = [j for j in self._jobs if j.status == JobStatus.PENDING]
        pending.sort(key=lambda j: (-j.priority.value, j.created_at))
        return pending

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
    """File-based exclusive lock for drain ownership.

    Uses fcntl on POSIX, msvcrt on Windows.
    """

    def __init__(self, path: Path) -> None:
        self._path = path
        self._fd: int | None = None

    def acquire(self) -> bool:
        self._path.parent.mkdir(parents=True, exist_ok=True)
        self._fd = os.open(str(self._path), os.O_CREAT | os.O_RDWR)
        try:
            if sys.platform == "win32":
                import msvcrt
                msvcrt.locking(self._fd, msvcrt.LK_NBLCK, 1)
            else:
                import fcntl
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
            if sys.platform == "win32":
                import msvcrt
                with contextlib.suppress(OSError):
                    msvcrt.locking(self._fd, msvcrt.LK_UNLCK, 1)
            else:
                import fcntl
                fcntl.flock(self._fd, fcntl.LOCK_UN)
            os.close(self._fd)
            self._fd = None

    def __enter__(self) -> _DrainLock:
        return self

    def __exit__(self, *args: Any) -> None:
        self.release()


# ---- Serialization ----


def _retry_replace_on_windows(src: Path, dst: Path) -> None:
    """Rename `src` onto `dst` with Windows file-share backoff.

    POSIX: first attempt always succeeds (atomic, no share modes).
    Windows: MoveFileEx can fail with WinError 5 (Access denied)
    while a peer writer is mid-rename or the target is briefly open.

    Backoff is linear-ish (0.05s, 0.10s, …) with random jitter so
    N concurrent writers don't retry in lockstep and collide on
    every attempt — the #175 flake. Without jitter, three workers
    that contended at t=0 will also contend at t=0.05s, t=0.15s,
    etc., and the retry budget buys nothing. 18 attempts over ~8s
    of wall time — long enough to outlast real CI-runner
    contention, short enough that a genuinely denied target still
    surfaces before the test timeout.
    """
    if sys.platform != "win32":
        src.replace(dst)
        return
    import random as _random
    import time as _time

    last_exc: PermissionError | None = None
    for attempt in range(18):
        try:
            src.replace(dst)
            return
        except PermissionError as exc:
            last_exc = exc
            base = 0.05 * (attempt + 1)
            jitter = _random.uniform(0.0, base)
            _time.sleep(base + jitter)
    assert last_exc is not None
    raise last_exc


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
                phase=rd.get("phase"),
                last_output_at=datetime.fromisoformat(rd["last_output_at"]) if rd.get("last_output_at") else None,
                last_heartbeat_at=(
                    datetime.fromisoformat(rd["last_heartbeat_at"])
                    if rd.get("last_heartbeat_at")
                    else None
                ),
                quiet_for_secs=rd.get("quiet_for_secs"),
                liveness=rd.get("liveness"),
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

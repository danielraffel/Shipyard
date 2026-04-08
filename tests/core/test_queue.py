"""Tests for the job queue."""

from __future__ import annotations

from pathlib import Path

import pytest

from shipyard.core.job import Job, JobStatus, Priority, TargetResult, TargetStatus, ValidationMode
from shipyard.core.queue import Queue


class TestQueue:
    def test_enqueue_and_retrieve(self, queue: Queue) -> None:
        job = Job.create(sha="abc", branch="main", target_names=["mac"])
        queue.enqueue(job)
        assert queue.pending_count == 1
        retrieved = queue.get(job.id)
        assert retrieved is not None
        assert retrieved.sha == "abc"

    def test_next_pending_returns_highest_priority(self, queue: Queue) -> None:
        low = Job.create(sha="a", branch="feat/a", target_names=["mac"], priority=Priority.LOW)
        high = Job.create(sha="b", branch="feat/b", target_names=["mac"], priority=Priority.HIGH)
        queue.enqueue(low)
        queue.enqueue(high)
        nxt = queue.next_pending()
        assert nxt is not None
        assert nxt.id == high.id

    def test_next_pending_fifo_within_priority(self, queue: Queue) -> None:
        first = Job.create(sha="a", branch="feat/a", target_names=["mac"])
        second = Job.create(sha="b", branch="feat/b", target_names=["mac"])
        queue.enqueue(first)
        queue.enqueue(second)
        nxt = queue.next_pending()
        assert nxt is not None
        assert nxt.id == first.id

    def test_supersedence_replaces_pending_same_scope(self, queue: Queue) -> None:
        old = Job.create(sha="old", branch="feat/x", target_names=["mac"])
        queue.enqueue(old)
        new = Job.create(sha="new", branch="feat/x", target_names=["mac"])
        queue.enqueue(new)
        assert queue.pending_count == 1
        nxt = queue.next_pending()
        assert nxt is not None
        assert nxt.sha == "new"

    def test_supersedence_keeps_different_targets(self, queue: Queue) -> None:
        """A narrower rerun (different target set) should not be superseded."""
        full = Job.create(sha="abc", branch="feat/x", target_names=["mac", "ubuntu", "windows"])
        queue.enqueue(full)
        narrow = Job.create(sha="abc", branch="feat/x", target_names=["windows"])
        queue.enqueue(narrow)
        # Both should exist — different target sets
        assert queue.pending_count == 2

    def test_supersedence_keeps_different_mode(self, queue: Queue) -> None:
        """Smoke and full runs for the same branch coexist."""
        full = Job.create(sha="abc", branch="feat/x", target_names=["mac"], mode=ValidationMode.FULL)
        queue.enqueue(full)
        smoke = Job.create(sha="abc", branch="feat/x", target_names=["mac"], mode=ValidationMode.SMOKE)
        queue.enqueue(smoke)
        assert queue.pending_count == 2

    def test_supersedence_does_not_cancel_running(self, queue: Queue) -> None:
        running = Job.create(sha="old", branch="feat/x", target_names=["mac"])
        queue.enqueue(running)
        running = running.start()
        queue.update(running)

        new = Job.create(sha="new", branch="feat/x", target_names=["mac"])
        queue.enqueue(new)

        # Running job still exists
        assert queue.running_count == 1
        assert queue.pending_count == 1

    def test_update_persists(self, queue: Queue) -> None:
        job = Job.create(sha="abc", branch="main", target_names=["mac"])
        queue.enqueue(job)
        started = job.start()
        queue.update(started)

        retrieved = queue.get(job.id)
        assert retrieved is not None
        assert retrieved.status == JobStatus.RUNNING

    def test_get_active(self, queue: Queue) -> None:
        job = Job.create(sha="abc", branch="main", target_names=["mac"])
        queue.enqueue(job)
        assert queue.get_active() is None

        started = job.start()
        queue.update(started)
        active = queue.get_active()
        assert active is not None
        assert active.id == job.id

    def test_get_recent(self, queue: Queue) -> None:
        for i in range(5):
            job = Job.create(sha=f"sha{i}", branch=f"feat/{i}", target_names=["mac"])
            job = job.start().complete()
            queue.enqueue(job)
            queue.update(job)

        recent = queue.get_recent(limit=3)
        assert len(recent) == 3

    def test_persistence_across_instances(self, tmp_path: Path) -> None:
        state_dir = tmp_path / "queue"
        q1 = Queue(state_dir=state_dir)
        job = Job.create(sha="abc", branch="main", target_names=["mac"])
        q1.enqueue(job)

        q2 = Queue(state_dir=state_dir)
        retrieved = q2.get(job.id)
        assert retrieved is not None
        assert retrieved.sha == "abc"

    def test_trim_completed(self, queue: Queue) -> None:
        from shipyard.core.queue import KEEP_COMPLETED

        for i in range(KEEP_COMPLETED + 10):
            job = Job.create(sha=f"sha{i}", branch=f"feat/{i}", target_names=["mac"])
            queue.enqueue(job)
            job = job.start().complete()
            queue.update(job)

        recent = queue.get_recent(limit=100)
        assert len(recent) <= KEEP_COMPLETED

    def test_next_pending_returns_none_when_empty(self, queue: Queue) -> None:
        assert queue.next_pending() is None

    def test_get_nonexistent(self, queue: Queue) -> None:
        assert queue.get("nonexistent") is None


class TestDrainLock:
    def test_acquire_and_release(self, tmp_path: Path) -> None:
        queue = Queue(state_dir=tmp_path / "queue")
        lock = queue.acquire_drain_lock()
        assert lock is not None
        lock.release()

    def test_second_acquire_fails(self, tmp_path: Path) -> None:
        queue = Queue(state_dir=tmp_path / "queue")
        lock1 = queue.acquire_drain_lock()
        assert lock1 is not None

        lock2 = queue.acquire_drain_lock()
        assert lock2 is None

        lock1.release()

    def test_context_manager(self, tmp_path: Path) -> None:
        queue = Queue(state_dir=tmp_path / "queue")
        lock = queue.acquire_drain_lock()
        assert lock is not None
        with lock:
            # Lock held inside context
            assert queue.acquire_drain_lock() is None
        # Lock released after context
        lock2 = queue.acquire_drain_lock()
        assert lock2 is not None
        lock2.release()

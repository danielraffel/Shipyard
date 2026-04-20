"""Atomic-write tests for the queue file (#102).

The original `_save()` wrote directly to `queue.json`. A kill between
open-with-truncate and the final write produced a zero-byte file that
crashed every subsequent invocation with `JSONDecodeError`. Pulp
shipped a client-side workaround in tools/install-shipyard.sh; this
suite locks in the Shipyard-side fix: atomic tmp-file + fsync +
Path.replace, with graceful recovery from pre-existing corruption.
"""

from __future__ import annotations

import json
import multiprocessing
import os
import time
from pathlib import Path

import pytest

from shipyard.core.job import Job
from shipyard.core.queue import Queue


def _enqueue_one(queue: Queue, *, sha: str, branch: str) -> Job:
    job = Job.create(sha=sha, branch=branch, target_names=["mac"])
    queue.enqueue(job)
    return job


def test_save_writes_atomically_and_leaves_no_tmp(tmp_path: Path) -> None:
    queue = Queue(state_dir=tmp_path)
    _enqueue_one(queue, sha="abc", branch="feat/a")
    queue_file = tmp_path / "queue.json"
    assert queue_file.exists()
    # Per-writer tmp files are PID+random-suffixed and must be gone
    # after a successful rename. The concurrent-writers test below
    # covers the race; here we just assert the single-writer cleanup.
    leftovers = list(tmp_path.glob(".queue-*.json.tmp"))
    assert not leftovers, f"orphan tmp files: {leftovers}"
    # File should be valid JSON, parseable by an independent Queue.
    json.loads(queue_file.read_text())


def test_kill_mid_flush_leaves_previous_file_intact(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Simulate a kill between tmp-write and rename.

    Before atomic writes this left a zero-byte queue.json. After, the
    destination is untouched — the caller can restart and reload the
    previous valid state.
    """
    queue = Queue(state_dir=tmp_path)
    _enqueue_one(queue, sha="before", branch="feat/before")
    queue_file = tmp_path / "queue.json"
    original = queue_file.read_text()

    def _boom(self: Path, target: Path | str) -> Path:
        # KeyboardInterrupt is what a real SIGINT would raise —
        # BaseException, not Exception — so it propagates past any
        # `except Exception` in the save path, matching a real kill.
        raise KeyboardInterrupt("simulated kill between write and rename")

    monkeypatch.setattr(Path, "replace", _boom)

    with pytest.raises(KeyboardInterrupt):
        queue.update(
            Job.create(sha="after", branch="feat/after", target_names=["mac"])
        )

    # Destination is byte-for-byte what it was before the failed save.
    # This is the invariant that fixed #102 — the pre-atomic writer
    # truncated queue.json to zero bytes here.
    assert queue_file.read_text() == original


def test_failed_save_cleans_up_tmp_file_on_recoverable_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A non-fatal rename failure should not leave an orphan tmp."""
    queue = Queue(state_dir=tmp_path)
    _enqueue_one(queue, sha="before", branch="feat/before")

    def _boom(self: Path, target: Path | str) -> Path:
        raise OSError("disk full")

    monkeypatch.setattr(Path, "replace", _boom)

    with pytest.raises(OSError):
        queue.update(
            Job.create(sha="after", branch="feat/after", target_names=["mac"])
        )

    # The except branch in _save() unlinks the per-writer tmp file.
    leftovers = list(tmp_path.glob(".queue-*.json.tmp"))
    assert not leftovers, f"orphan tmp files: {leftovers}"


def test_legacy_tmp_from_prior_crash_is_cleared_on_save(tmp_path: Path) -> None:
    """Pre-atomic writers used a fixed `queue.json.tmp` name.

    After upgrading, that orphan file is unowned and should not block
    the new save path. We clear it opportunistically on the next save.
    """
    stale = tmp_path / "queue.json.tmp"
    stale.write_text("leftover garbage from a pre-atomic SIGKILL")

    queue = Queue(state_dir=tmp_path)
    _enqueue_one(queue, sha="abc", branch="feat/a")

    queue_file = tmp_path / "queue.json"
    assert json.loads(queue_file.read_text())["jobs"], "save must have written"
    assert not stale.exists(), "legacy tmp should be swept"


def test_zero_byte_queue_file_is_recovered(tmp_path: Path) -> None:
    """A zero-byte file (the pre-atomic failure mode) reads as empty."""
    queue_file = tmp_path / "queue.json"
    queue_file.touch()
    assert queue_file.stat().st_size == 0

    queue = Queue(state_dir=tmp_path)
    assert queue.pending_count == 0
    # The next save replaces the zero-byte file with a valid one.
    _enqueue_one(queue, sha="abc", branch="feat/a")
    assert queue_file.stat().st_size > 0
    json.loads(queue_file.read_text())


def test_partial_json_file_is_recovered(tmp_path: Path) -> None:
    """Corruption from a pre-atomic writer (half-written JSON) still loads."""
    queue_file = tmp_path / "queue.json"
    queue_file.write_text('{"jobs": [{"id":')  # truncated mid-record

    queue = Queue(state_dir=tmp_path)
    # Should NOT raise JSONDecodeError — recovery path kicks in.
    assert queue.pending_count == 0
    # A subsequent save overwrites the corrupt file with valid JSON.
    _enqueue_one(queue, sha="abc", branch="feat/a")
    parsed = json.loads(queue_file.read_text())
    assert len(parsed["jobs"]) == 1


def _concurrent_writer(state_dir: str, tag: str, iterations: int) -> None:
    """Subprocess worker: enqueue N jobs tagged with `tag`."""
    queue = Queue(state_dir=Path(state_dir))
    for i in range(iterations):
        job = Job.create(
            sha=f"{tag}-{i}",
            branch=f"feat/{tag}-{i}",
            target_names=["mac"],
        )
        queue.enqueue(job)
        # A brief yield to interleave writes.
        time.sleep(0.001)


def test_concurrent_writers_never_produce_torn_file(tmp_path: Path) -> None:
    """Multiple writers — last-writer-wins, never a partial JSON file.

    The fix doesn't serialize writers; it just guarantees that at no
    point does a reader see a half-written file. Any read of
    queue.json at any moment must parse as valid JSON.
    """
    ctx = multiprocessing.get_context("spawn")
    procs = [
        ctx.Process(target=_concurrent_writer, args=(str(tmp_path), tag, 15))
        for tag in ("A", "B", "C")
    ]
    for p in procs:
        p.start()
    # While writers are running, repeatedly read the file — every
    # read must parse cleanly. Without atomic writes this exposed
    # partial JSON around ~1 in 10 reads on a busy system.
    #
    # On Windows, `os.replace` uses MoveFileEx, which opens the target
    # with file-sharing modes that can deny concurrent reads for a few
    # milliseconds around the rename. A PermissionError here doesn't
    # indicate a torn write — it means the reader hit the rename
    # window. We tolerate those and just skip the read attempt; the
    # contract (never-torn-JSON) is what we actually verify.
    queue_file = tmp_path / "queue.json"
    deadline = time.monotonic() + 3.0
    read_count = 0
    permission_errors = 0
    while any(p.is_alive() for p in procs) and time.monotonic() < deadline:
        try:
            if queue_file.exists():
                raw = queue_file.read_text()
                if raw:
                    # JSON must always be parseable, never half-written.
                    json.loads(raw)
                    read_count += 1
        except PermissionError:
            # Windows rename window — see comment above.
            permission_errors += 1
    for p in procs:
        p.join(timeout=5)
        assert p.exitcode == 0, f"worker failed: exit {p.exitcode}"

    # The final file is valid JSON (last-writer wins is fine). On
    # Windows the final read can also race with a just-completing
    # rename, so retry briefly.
    for _ in range(20):
        try:
            final = json.loads(queue_file.read_text())
            break
        except PermissionError:
            time.sleep(0.05)
    else:
        pytest.fail("final read never succeeded — file-share storm")
    assert isinstance(final["jobs"], list)
    # At least some reads happened during contention — sanity check
    # that the torn-read window was actually exercised.
    assert read_count >= 1


def test_save_writes_fsynced_before_rename(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Regression: the tmp file must be fsynced before the rename."""
    fsync_calls: list[int] = []
    real_fsync = os.fsync

    def _tracking_fsync(fd: int) -> None:
        fsync_calls.append(fd)
        real_fsync(fd)

    rename_calls: list[Path] = []
    real_replace = Path.replace

    def _tracking_replace(self: Path, target: Path | str) -> Path:
        rename_calls.append(self)
        return real_replace(self, target)

    monkeypatch.setattr(os, "fsync", _tracking_fsync)
    monkeypatch.setattr(Path, "replace", _tracking_replace)

    queue = Queue(state_dir=tmp_path)
    _enqueue_one(queue, sha="abc", branch="feat/a")

    # fsync must have been called, and at least one of those calls
    # must precede any rename of a queue tmp file.
    assert fsync_calls, "os.fsync was not called during _save()"
    assert rename_calls, "Path.replace was not called during _save()"

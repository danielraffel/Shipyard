from __future__ import annotations

import importlib.util
import fcntl
import os
import sys
import tempfile
import unittest
from argparse import Namespace
from pathlib import Path


SCRIPT = Path(__file__).with_name("sandbox_daemon_guardian.py")
SPEC = importlib.util.spec_from_file_location("sandbox_daemon_guardian", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
guardian = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = guardian
SPEC.loader.exec_module(guardian)


class GuardianLifecycleTests(unittest.TestCase):
    def make_guardian(self, root: Path) -> guardian.Guardian:
        return guardian.Guardian(
            Namespace(
                owner_pid=os.getpid(),
                installed=str(root / "installed"),
                candidate=str(root / "candidate"),
                global_dir=str(root / "global"),
                state_dir=str(root / "state"),
                canary_root=str(root),
                done_file=str(root / "done"),
                ready_receipt=str(root / "ready.json"),
                final_receipt=str(root / "final.json"),
                lease_dir=str(root / "lease"),
                production_pid_file=str(root / "production.pid"),
            )
        )

    def test_guardian_owns_cleanup_at_successful_atomic_acquisition(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            active = self.make_guardian(Path(directory))
            active.acquire()
            self.assertTrue(active.lease_owned)
            self.assertTrue(active.lease_dir.is_dir())
            active.release()
            self.assertFalse(active.lease_dir.exists())

    def test_contended_acquisition_never_claims_or_removes_foreign_lease(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            active = self.make_guardian(Path(directory))
            active.lease_dir.mkdir()
            with self.assertRaises(FileExistsError):
                active.acquire()
            self.assertFalse(active.lease_owned)
            self.assertTrue(active.lease_dir.is_dir())

    def test_lifetime_lock_proof_requires_real_lock_contention(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "writer.lock"
            with path.open("a+b") as holder:
                fcntl.flock(holder.fileno(), fcntl.LOCK_SH)
                self.assertTrue(guardian._exclusive_lock_is_contended(path))
                fcntl.flock(holder.fileno(), fcntl.LOCK_UN)
            self.assertFalse(guardian._exclusive_lock_is_contended(path))

    def test_owner_cancellation_stops_restores_then_releases(self) -> None:
        events: list[str] = []

        def cancelled() -> None:
            events.append("wait")
            raise guardian.OwnerEnded("cancelled")

        with self.assertRaises(guardian.OwnerEnded):
            guardian.run_lifecycle(
                lambda: events.append("acquire"),
                lambda: events.append("quiesce"),
                lambda: events.append("start"),
                cancelled,
                lambda: events.append("stop"),
                lambda: events.append("restore"),
                lambda: events.append("release"),
            )

        self.assertEqual(
            events,
            ["acquire", "quiesce", "start", "wait", "stop", "restore", "release"],
        )

    def test_preflight_failure_releases_without_false_restore(self) -> None:
        events: list[str] = []

        def failed_preflight() -> None:
            events.append("quiesce")
            raise guardian.GuardianError("preflight")

        with self.assertRaises(guardian.GuardianError):
            guardian.run_lifecycle(
                lambda: events.append("acquire"),
                failed_preflight,
                lambda: events.append("start"),
                lambda: events.append("wait"),
                lambda: events.append("stop"),
                lambda: events.append("restore"),
                lambda: events.append("release"),
            )

        self.assertEqual(events, ["acquire", "quiesce", "release"])

    def test_lease_creation_failure_has_no_cleanup_claim(self) -> None:
        events: list[str] = []

        def failed_acquire() -> None:
            events.append("acquire")
            raise FileExistsError("owned elsewhere")

        with self.assertRaises(FileExistsError):
            guardian.run_lifecycle(
                failed_acquire,
                lambda: events.append("quiesce"),
                lambda: events.append("start"),
                lambda: events.append("wait"),
                lambda: events.append("stop"),
                lambda: events.append("restore"),
                lambda: events.append("release"),
            )

        self.assertEqual(events, ["acquire"])

    def test_failed_candidate_stop_cannot_skip_restore_or_release(self) -> None:
        events: list[str] = []

        def failed_stop() -> None:
            events.append("stop")
            raise guardian.GuardianError("stubborn candidate")

        with self.assertRaisesRegex(
            guardian.GuardianError, "stop candidate.*stubborn candidate"
        ):
            guardian.run_lifecycle(
                lambda: events.append("acquire"),
                lambda: events.append("quiesce"),
                lambda: events.append("start"),
                lambda: events.append("wait"),
                failed_stop,
                lambda: events.append("restore"),
                lambda: events.append("release"),
            )

        self.assertEqual(
            events,
            ["acquire", "quiesce", "start", "wait", "stop", "restore", "release"],
        )

    def test_failed_restore_cannot_skip_lease_release(self) -> None:
        events: list[str] = []

        def failed_restore() -> None:
            events.append("restore")
            raise guardian.GuardianError("restore failed")

        with self.assertRaisesRegex(
            guardian.GuardianError, "restore production.*restore failed"
        ):
            guardian.run_lifecycle(
                lambda: events.append("acquire"),
                lambda: events.append("quiesce"),
                lambda: events.append("start"),
                lambda: events.append("wait"),
                lambda: events.append("stop"),
                failed_restore,
                lambda: events.append("release"),
            )

        self.assertEqual(
            events,
            ["acquire", "quiesce", "start", "wait", "stop", "restore", "release"],
        )

    def test_repo_and_mode_authority_come_from_exact_argv(self) -> None:
        argv = (
            "/opt/shipyard",
            "--mode",
            "shipyard",
            "daemon",
            "run",
            "--repo",
            "owner/z",
            "--repo",
            "owner/a",
            "--repo",
            "owner/z",
        )
        self.assertEqual(guardian._mode_arg(argv), "shipyard")
        self.assertEqual(guardian._repo_args(argv), ("owner/a", "owner/z"))


if __name__ == "__main__":
    unittest.main()

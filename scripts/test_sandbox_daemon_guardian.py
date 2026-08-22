from __future__ import annotations

import importlib.util
import contextlib
import fcntl
import os
import sys
import tempfile
import unittest
from argparse import Namespace
from pathlib import Path
from unittest import mock


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

    def test_finalize_wait_accepts_a_transient_production_mutation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "writer.lock"
            with contextlib.ExitStack() as stack:
                holders = stack.enter_context(
                    mock.patch.object(
                        guardian,
                        "_lock_holders",
                        side_effect=[(4242,), (4242,), (4242,), (4242,)],
                    )
                )
                contention = stack.enter_context(
                    mock.patch.object(
                        guardian,
                        "_exclusive_lock_is_contended",
                        side_effect=[True, False, False, False],
                    )
                )
                stack.enter_context(mock.patch.object(guardian.time, "sleep"))
                guardian._wait_for_idle_writer_domain(path, 4242)

            self.assertEqual(holders.call_count, 4)
            self.assertEqual(contention.call_count, 4)

    def test_finalize_wait_rejects_a_retained_lifetime_lock(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "writer.lock"
            with contextlib.ExitStack() as stack:
                stack.enter_context(
                    mock.patch.object(
                        guardian, "_lock_holders", return_value=(4242,)
                    )
                )
                stack.enter_context(
                    mock.patch.object(
                        guardian, "_exclusive_lock_is_contended", return_value=True
                    )
                )
                stack.enter_context(
                    mock.patch.object(
                        guardian.time, "monotonic", side_effect=[0.0, 11.0]
                    )
                )
                stack.enter_context(mock.patch.object(guardian.time, "sleep"))
                with self.assertRaisesRegex(
                    guardian.GuardianError, "retained the writer-domain lock"
                ):
                    guardian._wait_for_idle_writer_domain(path, 4242)

    def test_finalize_wait_rejects_a_foreign_holder_immediately(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "writer.lock"
            with mock.patch.object(
                guardian, "_lock_holders", return_value=(4242, 7331)
            ), mock.patch.object(
                guardian, "_exclusive_lock_is_contended"
            ) as contention:
                with self.assertRaisesRegex(
                    guardian.GuardianError, "foreign process entered"
                ):
                    guardian._wait_for_idle_writer_domain(path, 4242)
            contention.assert_not_called()

    def test_post_cutover_idle_daemon_selects_preserve_and_fence_path(self) -> None:
        for holders in [(), (4242,)]:
            with self.subTest(holders=holders):
                self.assertEqual(
                    guardian._select_transition(4242, holders, False),
                    guardian.CORRECTED_TRANSITION,
                )

    def test_post_cutover_preflight_never_stops_idle_corrected_daemon(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            active.installed.write_bytes(b"installed")
            active.candidate.write_bytes(b"candidate")
            active.production_pid_file.write_text("4242\n", encoding="utf-8")
            snapshot = guardian.ProcessSnapshot(
                pid=4242,
                executable=str(active.installed),
                argv=(
                    str(active.installed),
                    "--mode",
                    "shipyard",
                    "daemon",
                    "run",
                    "--repo",
                    "owner/repo",
                ),
                environment={"HOME": str(root)},
                cwd=str(root),
                stdin_path="/dev/null",
                stdout_path="/dev/null",
                stderr_path="/dev/null",
                start_time="Sat Aug 22 00:00:00 2026",
            )
            with contextlib.ExitStack() as stack:
                stack.enter_context(
                    mock.patch.object(
                        guardian, "snapshot_process", return_value=snapshot
                    )
                )
                stack.enter_context(
                    mock.patch.object(
                        guardian, "_configured_repos", return_value=("owner/repo",)
                    )
                )
                stack.enter_context(
                    mock.patch.object(guardian, "_active_runs", return_value=())
                )
                stack.enter_context(
                    mock.patch.object(guardian, "_lock_holders", return_value=(4242,))
                )
                stack.enter_context(
                    mock.patch.object(
                        guardian, "_exclusive_lock_is_contended", return_value=False
                    )
                )
                run = stack.enter_context(mock.patch.object(guardian, "_run"))
                active.preflight_and_transition()

            self.assertEqual(active.transition_path, guardian.CORRECTED_TRANSITION)
            self.assertFalse(active.production_quiesced)
            self.assertFalse(active.old_lifetime_lock_owned)
            run.assert_not_called()

    def test_pre_cutover_lifetime_lock_selects_quiesce_restore_path(self) -> None:
        self.assertEqual(
            guardian._select_transition(4242, (4242,), True),
            guardian.LEGACY_TRANSITION,
        )

    def test_post_cutover_ambiguous_lock_state_fails_closed(self) -> None:
        for holders, contended in [
            ((), True),
            ((7331,), False),
            ((7331,), True),
            ((4242, 7331), True),
        ]:
            with self.subTest(holders=holders, contended=contended):
                with self.assertRaisesRegex(
                    guardian.GuardianError, "ambiguous production writer-domain state"
                ):
                    guardian._select_transition(4242, holders, contended)

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

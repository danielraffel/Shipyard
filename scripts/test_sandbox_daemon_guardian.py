from __future__ import annotations

import importlib.util
import contextlib
import fcntl
import json
import os
import subprocess
import sys
import tempfile
import time
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

    def test_holder_probe_distinguishes_no_match_from_lsof_failure(self) -> None:
        path = Path("/tmp/writer.lock")
        no_match = mock.Mock(returncode=1, stdout="", stderr="")
        with mock.patch.object(guardian.subprocess, "run", return_value=no_match):
            self.assertEqual(guardian._lock_holders(path), ())

        failed = mock.Mock(returncode=1, stdout="", stderr="permission denied")
        with mock.patch.object(guardian.subprocess, "run", return_value=failed):
            with self.assertRaisesRegex(
                guardian.GuardianError, "could not inspect writer-domain holders"
            ):
                guardian._lock_holders(path)

    def test_holder_probe_rejects_inconsistent_lsof_success(self) -> None:
        path = Path("/tmp/writer.lock")
        inconsistent = mock.Mock(returncode=0, stdout="", stderr="")
        with mock.patch.object(
            guardian.subprocess, "run", return_value=inconsistent
        ), self.assertRaisesRegex(guardian.GuardianError, "inconsistent lsof"):
            guardian._lock_holders(path)

    def test_holder_probe_revalidates_before_one_timeout_retry(self) -> None:
        path = Path("/tmp/writer.lock")
        revalidate = mock.Mock()
        timeout = subprocess.TimeoutExpired(["lsof"], 7)
        success = mock.Mock(returncode=0, stdout="p4242\nf3\n", stderr="")
        with mock.patch.object(
            guardian.subprocess, "run", side_effect=[timeout, success]
        ) as run:
            self.assertEqual(
                guardian._lock_holders(
                    path,
                    retry_after_timeout=revalidate,
                ),
                (4242,),
            )
        self.assertEqual(run.call_count, 2)
        self.assertEqual(
            run.call_args_list[0].args[0],
            [
                "/usr/sbin/lsof",
                "-nP",
                "-S",
                "2",
                "-F",
                "pf",
                "--",
                str(path),
            ],
        )
        revalidate.assert_called_once()
        retry_deadline = revalidate.call_args.args[0]
        self.assertGreater(retry_deadline, time.monotonic())
        self.assertLessEqual(
            retry_deadline - time.monotonic(), guardian.LOCK_HOLDER_TOTAL_TIMEOUT
        )

    def test_holder_probe_does_not_retry_when_revalidation_fails(self) -> None:
        path = Path("/tmp/writer.lock")
        timeout = subprocess.TimeoutExpired(["lsof"], 7)
        revalidation_failure = guardian.GuardianError("identity changed")
        revalidate = mock.Mock(side_effect=revalidation_failure)
        with mock.patch.object(
            guardian.subprocess, "run", side_effect=[timeout]
        ) as run, self.assertRaisesRegex(guardian.GuardianError, "identity changed"):
            guardian._lock_holders(path, retry_after_timeout=revalidate)
        run.assert_called_once()
        revalidate.assert_called_once()

    def test_holder_probe_retry_uses_remaining_aggregate_deadline(self) -> None:
        path = Path("/tmp/writer.lock")
        timeout = subprocess.TimeoutExpired(["lsof"], 7)
        success = mock.Mock(returncode=1, stdout="", stderr="")
        revalidate = mock.Mock()
        with mock.patch.object(
            guardian.time, "monotonic", side_effect=[100.0, 100.0, 110.0]
        ), mock.patch.object(
            guardian.subprocess, "run", side_effect=[timeout, success]
        ) as run:
            self.assertEqual(
                guardian._lock_holders(path, retry_after_timeout=revalidate), ()
            )
        revalidate.assert_called_once_with(115.0)
        self.assertEqual(run.call_args_list[0].kwargs["timeout"], 7.0)
        self.assertEqual(run.call_args_list[1].kwargs["timeout"], 5.0)

    def test_holder_probe_repeated_timeout_fails_closed_with_diagnostics(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "writer.lock"
            path.touch()
            revalidate = mock.Mock()
            timeout = subprocess.TimeoutExpired(["lsof"], 7)
            with mock.patch.object(
                guardian.subprocess, "run", side_effect=[timeout, timeout]
            ), self.assertRaisesRegex(
                guardian.GuardianError, "after 2 attempt"
            ):
                guardian._lock_holders(
                    path,
                    retry_after_timeout=revalidate,
                    diagnostic_root=root,
                )
            revalidate.assert_called_once()
            diagnostics = sorted(root.glob("lock-holder-timeout-*.json"))
            self.assertEqual(len(diagnostics), 2)
            payloads = [json.loads(item.read_text()) for item in diagnostics]
            self.assertEqual({payload["attempt"] for payload in payloads}, {1, 2})
            self.assertTrue(
                all(payload["lock_inode"] == path.stat().st_ino for payload in payloads)
            )

    def test_holder_probe_rejects_incomplete_structured_output(self) -> None:
        path = Path("/tmp/writer.lock")
        incomplete = mock.Mock(returncode=0, stdout="p4242\n", stderr="")
        with mock.patch.object(
            guardian.subprocess, "run", return_value=incomplete
        ), self.assertRaisesRegex(guardian.GuardianError, "incomplete lsof"):
            guardian._lock_holders(path)

    @unittest.skipUnless(sys.platform == "darwin", "macOS lsof proof")
    def test_holder_probe_observes_real_macos_holder_and_no_holder(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "writer.lock"
            with path.open("a+b") as holder:
                fcntl.flock(holder.fileno(), fcntl.LOCK_EX)
                self.assertEqual(guardian._lock_holders(path), (os.getpid(),))
                fcntl.flock(holder.fileno(), fcntl.LOCK_UN)
            self.assertEqual(guardian._lock_holders(path), ())

    def test_json_status_probe_does_not_inherit_protected_stdio_marker(self) -> None:
        snapshot = guardian.ProcessSnapshot(
            pid=4242,
            executable="/tmp/shipyard",
            argv=("/tmp/shipyard", "daemon", "run"),
            environment={
                "HOME": "/tmp/home",
                guardian.PROTECTED_STDIO_PATH_ENV: "/tmp/daemon.log",
            },
            cwd="/tmp",
            stdin_path="/dev/null",
            stdout_path="/tmp/daemon.log",
            stderr_path="/tmp/daemon.log",
            start_time="Sat Aug 22 00:00:00 2026",
        )
        completed = mock.Mock(returncode=0, stdout='{"running":true}', stderr="")

        with mock.patch.object(guardian, "_run", return_value=completed) as run:
            self.assertEqual(
                guardian._json_command(snapshot, Path("/tmp/shipyard"), "daemon", "status"),
                {"running": True},
            )

        self.assertNotIn(
            guardian.PROTECTED_STDIO_PATH_ENV,
            run.call_args.kwargs["env"],
        )
        self.assertEqual(run.call_args.kwargs["cwd"], "/")
        self.assertEqual(
            run.call_args.args[0][:4],
            ["/tmp/shipyard", "--cwd", "/", "--json"],
        )

    def test_deadline_bound_status_skips_unbounded_timeout_diagnostics(self) -> None:
        snapshot = guardian.ProcessSnapshot(
            pid=4242,
            executable="/tmp/shipyard",
            argv=("/tmp/shipyard", "daemon", "run"),
            environment={"HOME": "/tmp/home"},
            cwd="/tmp",
            stdin_path="/dev/null",
            stdout_path="/tmp/daemon.log",
            stderr_path="/tmp/daemon.log",
            start_time="Sat Aug 22 00:00:00 2026",
        )
        completed = mock.Mock(returncode=0, stdout='{"running":true}', stderr="")
        with mock.patch.object(
            guardian, "_run", return_value=completed
        ) as bounded_run, mock.patch.object(guardian, "_run_status_probe") as diagnostic:
            guardian._json_command(
                snapshot,
                Path("/tmp/shipyard"),
                "daemon",
                "status",
                deadline=time.monotonic() + 5,
                diagnostic_root=Path("/tmp/diagnostics"),
            )
        bounded_run.assert_called_once()
        diagnostic.assert_not_called()

    def test_timed_out_status_captures_live_processes_before_terminating_child(self) -> None:
        process = mock.Mock(pid=7331, returncode=None)
        process.communicate.side_effect = [
            guardian.subprocess.TimeoutExpired(["shipyard"], 15.0),
            ("", "terminated"),
        ]
        evidence = Path("/tmp/status-timeout-7331")
        events: list[str] = []
        process.terminate.side_effect = lambda: events.append("terminate")

        with mock.patch.object(
            guardian.subprocess, "Popen", return_value=process
        ), mock.patch.object(
            guardian,
            "_capture_status_timeout",
            side_effect=lambda *_args: events.append("capture") or evidence,
        ):
            with self.assertRaises(guardian.subprocess.TimeoutExpired) as raised:
                guardian._run_status_probe(
                    ["shipyard", "--json", "daemon", "status"],
                    cwd="/tmp",
                    env={"HOME": "/tmp/home"},
                    timeout=15.0,
                    diagnostic_root=Path("/tmp/canary"),
                    production_pid=4242,
                )

        self.assertEqual(events, ["capture", "terminate"])
        self.assertIn(str(evidence), raised.exception.stderr)
        process.kill.assert_not_called()

    def test_status_timeout_diagnostic_failure_does_not_skip_child_cleanup(self) -> None:
        process = mock.Mock(pid=7331, returncode=None)
        process.communicate.side_effect = [
            guardian.subprocess.TimeoutExpired(["shipyard"], 15.0),
            ("", "terminated"),
        ]

        with mock.patch.object(
            guardian.subprocess, "Popen", return_value=process
        ), mock.patch.object(
            guardian,
            "_capture_status_timeout",
            side_effect=OSError("sample unavailable"),
        ):
            with self.assertRaises(guardian.subprocess.TimeoutExpired) as raised:
                guardian._run_status_probe(
                    ["shipyard", "--json", "daemon", "status"],
                    cwd="/tmp",
                    env={"HOME": "/tmp/home"},
                    timeout=15.0,
                    diagnostic_root=Path("/tmp/canary"),
                    production_pid=4242,
                )

        # A diagnostic helper defect must never leave the timed-out child alive.
        process.terminate.assert_called_once_with()
        self.assertIn("diagnostic capture failed", raised.exception.stderr)

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

    def test_ambiguous_no_holder_wait_accepts_only_stable_idle(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "writer.lock"
            verified: list[str] = []
            with contextlib.ExitStack() as stack:
                holders = stack.enter_context(
                    mock.patch.object(
                        guardian, "_lock_holders", side_effect=[(), (), (), ()]
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
                guardian._wait_for_idle_writer_domain(
                    path,
                    4242,
                    verify_production=lambda _deadline: verified.append("verified"),
                )

            self.assertEqual(holders.call_count, 4)
            self.assertEqual(contention.call_count, 4)
            self.assertEqual(len(verified), 4)

    def test_ambiguous_no_holder_wait_rejects_persistent_contention(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "writer.lock"
            with contextlib.ExitStack() as stack:
                stack.enter_context(
                    mock.patch.object(guardian, "_lock_holders", return_value=())
                )
                stack.enter_context(
                    mock.patch.object(
                        guardian, "_exclusive_lock_is_contended", return_value=True
                    )
                )
                stack.enter_context(
                    mock.patch.object(
                        guardian.time,
                        "monotonic",
                        side_effect=[0.0, 0.0, 0.0, 11.0],
                    )
                )
                stack.enter_context(mock.patch.object(guardian.time, "sleep"))
                with self.assertRaisesRegex(
                    guardian.GuardianError, "retained the writer-domain lock"
                ):
                    guardian._wait_for_idle_writer_domain(
                        path, 4242
                    )

    def test_ambiguous_no_holder_wait_rejects_identity_drift(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "writer.lock"
            verify = mock.Mock(
                side_effect=[None, guardian.GuardianError("production identity drift")]
            )
            with mock.patch.object(
                guardian, "_lock_holders", return_value=()
            ), mock.patch.object(
                guardian, "_exclusive_lock_is_contended", return_value=False
            ), mock.patch.object(guardian.time, "sleep"):
                with self.assertRaisesRegex(
                    guardian.GuardianError, "production identity drift"
                ):
                    guardian._wait_for_idle_writer_domain(
                        path,
                        4242,
                        verify_production=verify,
                    )

    def test_ambiguous_no_holder_wait_accepts_transient_production_holder(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "writer.lock"
            with mock.patch.object(
                guardian,
                "_lock_holders",
                side_effect=[(), (4242,), (), (), ()],
            ), mock.patch.object(
                guardian,
                "_exclusive_lock_is_contended",
                side_effect=[False, True, False, False, False],
            ), mock.patch.object(guardian.time, "sleep"):
                guardian._wait_for_idle_writer_domain(path, 4242)

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
                        guardian.time,
                        "monotonic",
                        side_effect=[0.0, 0.0, 0.0, 11.0],
                    )
                )
                stack.enter_context(mock.patch.object(guardian.time, "sleep"))
                with self.assertRaisesRegex(
                    guardian.RetainedLifetimeLock,
                    "retained the writer-domain lock",
                ):
                    guardian._wait_for_idle_writer_domain(path, 4242)

    def test_idle_then_reacquired_until_timeout_is_not_a_lifetime_lock(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "writer.lock"
            with mock.patch.object(
                guardian, "_lock_holders", side_effect=[(), (4242,)]
            ), mock.patch.object(
                guardian,
                "_exclusive_lock_is_contended",
                side_effect=[False, True],
            ), mock.patch.object(
                guardian.time,
                "monotonic",
                side_effect=[0.0, 0.0, 0.0, 0.0, 11.0],
            ), mock.patch.object(guardian.time, "sleep"):
                with self.assertRaises(guardian.GuardianError) as raised:
                    guardian._wait_for_idle_writer_domain(path, 4242)

            self.assertNotIsInstance(raised.exception, guardian.RetainedLifetimeLock)

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

    def test_preflight_routes_production_holder_contention_through_bounded_wait(
        self,
    ) -> None:
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
                    mock.patch.object(
                        guardian, "_lock_holders", return_value=(4242,)
                    )
                )
                stack.enter_context(
                    mock.patch.object(
                        guardian, "_exclusive_lock_is_contended", return_value=True
                    )
                )
                wait = stack.enter_context(
                    mock.patch.object(guardian, "_wait_for_idle_writer_domain")
                )
                run = stack.enter_context(mock.patch.object(guardian, "_run"))
                active.preflight_and_transition()

            self.assertEqual(active.transition_path, guardian.CORRECTED_TRANSITION)
            wait.assert_called_once_with(
                active.lock_path,
                snapshot.pid,
                verify_production=active.verify_unchanged_production,
                diagnostic_root=active.root,
            )
            run.assert_not_called()

    def test_preflight_classifies_only_persistent_production_holder_as_legacy(
        self,
    ) -> None:
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
                    str(active.installed), "--mode", "shipyard", "daemon", "run",
                    "--repo", "owner/repo",
                ),
                environment={"HOME": str(root)},
                cwd=str(root),
                stdin_path="/dev/null",
                stdout_path="/dev/null",
                stderr_path="/dev/null",
                start_time="Sat Aug 22 00:00:00 2026",
            )
            with mock.patch.object(
                guardian, "snapshot_process", return_value=snapshot
            ), mock.patch.object(
                guardian, "_configured_repos", return_value=("owner/repo",)
            ), mock.patch.object(
                guardian, "_active_runs", return_value=()
            ), mock.patch.object(
                guardian, "_lock_holders", side_effect=[(4242,), ()]
            ), mock.patch.object(
                guardian, "_exclusive_lock_is_contended", side_effect=[True, False]
            ), mock.patch.object(
                guardian,
                "_wait_for_idle_writer_domain",
                side_effect=guardian.RetainedLifetimeLock("persistent"),
            ), mock.patch.object(
                guardian, "_pid_alive", return_value=False
            ), mock.patch.object(guardian, "_run") as run:
                active.preflight_and_transition()

            self.assertEqual(active.transition_path, guardian.LEGACY_TRANSITION)
            self.assertTrue(active.production_quiesced)
            self.assertTrue(active.old_lifetime_lock_owned)
            run.assert_called_once()

    def test_preflight_contention_identity_drift_fails_without_stopping(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            active.installed.write_bytes(b"installed")
            active.candidate.write_bytes(b"candidate")
            active.production_pid_file.write_text("4242\n", encoding="utf-8")
            snapshot = guardian.ProcessSnapshot(
                pid=4242,
                executable=str(active.installed),
                argv=(str(active.installed), "--mode", "shipyard", "daemon", "run"),
                environment={"HOME": str(root)},
                cwd=str(root),
                stdin_path="/dev/null",
                stdout_path="/dev/null",
                stderr_path="/dev/null",
                start_time="Sat Aug 22 00:00:00 2026",
            )
            with mock.patch.object(
                guardian, "snapshot_process", return_value=snapshot
            ), mock.patch.object(
                guardian, "_configured_repos", return_value=()
            ), mock.patch.object(
                guardian, "_active_runs", return_value=()
            ), mock.patch.object(
                guardian, "_lock_holders", return_value=(4242,)
            ), mock.patch.object(
                guardian, "_exclusive_lock_is_contended", return_value=True
            ), mock.patch.object(
                guardian,
                "_wait_for_idle_writer_domain",
                side_effect=guardian.GuardianError("production identity drift"),
            ), mock.patch.object(guardian, "_run") as run:
                with self.assertRaisesRegex(
                    guardian.GuardianError, "production identity drift"
                ):
                    active.preflight_and_transition()

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

    def test_restore_retry_adopts_already_live_exact_daemon(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            active.installed.write_bytes(b"installed")
            active.installed_hash = guardian._sha256(active.installed)
            active.production_pid_file.write_text("7331\n", encoding="utf-8")
            active.lock_path = root / "writer.lock"
            active.configured_repos = ("owner/repo",)
            active.worker_ids = ()
            active.snapshot = guardian.ProcessSnapshot(
                pid=4242,
                executable=str(active.installed),
                argv=(str(active.installed), "--mode", "shipyard", "daemon", "run"),
                environment={"HOME": str(root)},
                cwd=str(root),
                stdin_path="/dev/null",
                stdout_path="/dev/null",
                stderr_path="/dev/null",
                start_time="old-start",
            )
            restored = guardian.ProcessSnapshot(
                **{**active.snapshot.__dict__, "pid": 7331, "start_time": "new-start"}
            )
            with mock.patch.object(
                guardian, "_pid_alive", return_value=True
            ), mock.patch.object(
                guardian, "snapshot_process", return_value=restored
            ), mock.patch.object(
                guardian, "_json_command", return_value={"running": True}
            ), mock.patch.object(
                guardian, "_configured_repos", return_value=("owner/repo",)
            ), mock.patch.object(
                guardian, "_active_runs", return_value=()
            ), mock.patch.object(
                guardian, "_lock_holders", return_value=(7331,)
            ), mock.patch.object(
                guardian, "_exclusive_lock_is_contended", return_value=True
            ), mock.patch.object(guardian.subprocess, "Popen") as popen:
                active.restore_legacy_production()

            popen.assert_not_called()
            self.assertEqual(active.restored_pid, 7331)
            self.assertTrue(active.production_identity_verified)

    def test_restore_retry_refuses_live_identity_drift_without_spawning(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            active.installed.write_bytes(b"installed")
            active.installed_hash = guardian._sha256(active.installed)
            active.production_pid_file.write_text("7331\n", encoding="utf-8")
            active.snapshot = guardian.ProcessSnapshot(
                pid=4242,
                executable=str(active.installed),
                argv=(str(active.installed), "--mode", "shipyard", "daemon", "run"),
                environment={"HOME": str(root)},
                cwd=str(root),
                stdin_path="/dev/null",
                stdout_path="/dev/null",
                stderr_path="/dev/null",
                start_time="old-start",
            )
            drifted = guardian.ProcessSnapshot(
                **{**active.snapshot.__dict__, "pid": 7331, "argv": ("foreign",)}
            )
            with mock.patch.object(
                guardian, "_pid_alive", return_value=True
            ), mock.patch.object(
                guardian, "snapshot_process", return_value=drifted
            ), mock.patch.object(guardian.subprocess, "Popen") as popen:
                with self.assertRaisesRegex(
                    guardian.GuardianError, "process identity differs"
                ):
                    active.restore_legacy_production()

            popen.assert_not_called()

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

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
        production_pid_file = root / "production-state" / "daemon" / "daemon.pid"
        production_pid_file.parent.mkdir(parents=True, exist_ok=True)
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
                production_pid_file=str(production_pid_file),
            )
        )

    def create_generation_lease(
        self,
        active: guardian.Guardian,
        *,
        phase: str = guardian.LEASE_PHASE_TRANSITIONING,
    ) -> tuple[os.stat_result, str]:
        metadata, generation = guardian._create_lease_generation(active.lease_dir)
        if phase == guardian.LEASE_PHASE_TRANSITIONING:
            metadata = guardian._advance_lease_generation(
                active.lease_dir,
                (metadata.st_dev, metadata.st_ino, metadata.st_ctime_ns),
                generation,
            )
        return metadata, generation

    def write_retained_owner_ended_evidence(
        self,
        active: guardian.Guardian,
        *,
        failure: str = guardian.RETAINED_OWNER_ENDED_FAILURE,
    ) -> tuple[
        Path,
        dict[str, object],
        dict[str, object],
        dict[str, object],
        os.stat_result,
        str,
    ]:
        active.lease_dir = active.root / "shipyard-sandbox-m3-lease"
        metadata, generation = self.create_generation_lease(active)
        prior = active.root / "shipyard-sandbox-m3-1-1"
        prior.mkdir(mode=0o700)
        installed_sha256 = "a" * 64
        candidate_sha256 = "b" * 64
        argv_sha256 = "c" * 64
        environment_sha256 = "d" * 64
        production_pid = 4242
        production_start_time = "Sat Aug 29 05:44:36 2026"
        candidate_pid = 7331
        receipt = {
            "schema_version": 1,
            "reason": "failed",
            "failure": failure,
            "candidate_stopped": True,
            "production_quiesced": False,
            "production_restored": False,
            "production_preserved": False,
            "production_identity_verified": False,
            "lease_retained": True,
            "lease_device": metadata.st_dev,
            "lease_inode": metadata.st_ino,
            "lease_ctime_ns": metadata.st_ctime_ns,
            "lease_generation": generation,
            "transition_path": guardian.CORRECTED_TRANSITION,
            "old_production_pid": production_pid,
            "old_production_start_time": production_start_time,
            "restored_production_pid": None,
            "final_production_pid": None,
            "final_production_start_time": None,
            "installed_sha256": installed_sha256,
            "candidate_sha256": candidate_sha256,
            "argv_sha256": argv_sha256,
            "environment_sha256": environment_sha256,
            "cwd": str(active.root),
            "mode": "shipyard",
            "configured_repos": ["owner/repo"],
            "active_runs": [],
            "old_lifetime_lock_owned": False,
            "mutation_fence_proved": True,
            "mutation_guard_path": str(active.mutation_guard_path),
            "mutation_probe_output": str(active.mutation_probe_output),
            "lease_removed": False,
            "reconciled_prior_canary_root": None,
        }
        ready = {
            "schema_version": 1,
            "phase": "ready",
            "candidate_pid": candidate_pid,
            "candidate_sha256": candidate_sha256,
            "installed_sha256": installed_sha256,
            "production_pid": production_pid,
            "production_start_time": production_start_time,
            "transition_path": guardian.CORRECTED_TRANSITION,
            "production_executable": str(active.installed),
            "production_argv_sha256": argv_sha256,
            "production_environment_sha256": environment_sha256,
            "production_cwd": str(active.root),
            "mutation_guard_path": str(active.mutation_guard_path),
            "configured_repos": ["owner/repo"],
            "active_runs": [],
        }
        mutation = {
            "schema_version": 1,
            "transition_path": guardian.CORRECTED_TRANSITION,
            "selected_protected_path": str(active.mutation_guard_path),
            "probe_output": str(active.mutation_probe_output),
            "production_pid": production_pid,
            "production_start_time": production_start_time,
            "installed_sha256": installed_sha256,
            "argv_sha256": argv_sha256,
            "environment_sha256": environment_sha256,
            "cwd": str(active.root),
            "returncode": guardian.WRITER_DOMAIN_OVERLAP_EXIT_CODE,
            "overlap_classification": guardian.WRITER_DOMAIN_OVERLAP_CLASSIFICATION,
            "mutation_absent": True,
            "selected_path_absent": True,
            "production_identity_preserved": True,
        }
        guardian._atomic_json(prior / "guardian-receipt.json", receipt)
        guardian._atomic_json(prior / "ready.json", ready)
        guardian._atomic_json(prior / "mutation-fence.json", mutation)
        return prior, receipt, ready, mutation, metadata, generation

    def test_guardian_owns_cleanup_at_successful_atomic_acquisition(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            active = self.make_guardian(Path(directory))
            active.acquire()
            self.assertTrue(active.lease_owned)
            self.assertTrue(active.lease_dir.is_dir())
            lease_stat = active.lease_dir.stat()
            self.assertEqual(active.lease_device, lease_stat.st_dev)
            self.assertEqual(active.lease_inode, lease_stat.st_ino)
            self.assertEqual(active.lease_ctime_ns, lease_stat.st_ctime_ns)
            _, observed_generation = guardian._validate_lease_generation(
                active.lease_dir
            )
            self.assertEqual(active.lease_generation, observed_generation)
            active.release()
            self.assertFalse(active.lease_dir.exists())
            self.assertFalse(
                active.lease_dir.with_name(
                    f".{active.lease_dir.name}.removed-{observed_generation}"
                ).exists()
            )

    def test_release_refuses_preexisting_generation_tombstone(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            active = self.make_guardian(Path(directory))
            active.production_preserved = True
            active.production_identity_verified = True
            active.acquire()
            tombstone = active.lease_dir.with_name(
                f".{active.lease_dir.name}.removed-{active.lease_generation}"
            )
            tombstone.mkdir(mode=0o700)

            with self.assertRaisesRegex(
                guardian.GuardianError, "removal tombstone already exists"
            ):
                active.release()

            self.assertTrue(active.lease_dir.is_dir())
            self.assertTrue(active.lease_owned)

    def test_release_refuses_substituted_detached_tombstone(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            active.production_preserved = True
            active.production_identity_verified = True
            active.acquire()
            generation = active.lease_generation
            self.assertIsInstance(generation, str)
            tombstone = active.lease_dir.with_name(
                f".{active.lease_dir.name}.removed-{generation}"
            )
            preserved = root / "detached-original"
            real_rename = os.rename

            def substitute_after_detach(source: Path, destination: Path) -> None:
                real_rename(source, destination)
                real_rename(destination, preserved)
                destination.mkdir(mode=0o700)
                guardian._durable_atomic_json(
                    destination / guardian.LEASE_GENERATION_MARKER,
                    {
                        "schema_version": 1,
                        "generation": generation,
                        "phase": guardian.LEASE_PHASE_ACQUIRING,
                    },
                )

            with mock.patch.object(
                guardian.os, "rename", side_effect=substitute_after_detach
            ), self.assertRaisesRegex(
                guardian.GuardianError, "detached host lease identity changed"
            ):
                active.release()

            self.assertFalse(active.lease_dir.exists())
            self.assertTrue(preserved.is_dir())
            self.assertTrue(tombstone.is_dir())

    def test_creation_failure_before_marker_never_exposes_canonical_lease(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            active = self.make_guardian(Path(directory))
            with mock.patch.object(
                guardian,
                "_durable_atomic_json",
                side_effect=OSError("marker publication failed"),
            ), self.assertRaisesRegex(OSError, "marker publication failed"):
                guardian._create_lease_generation(active.lease_dir)

            self.assertFalse(active.lease_dir.exists())
            self.assertEqual(
                list(active.lease_dir.parent.glob(f".{active.lease_dir.name}.creating-*")),
                [],
            )

    def test_creation_failure_before_rename_never_exposes_canonical_lease(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            active = self.make_guardian(Path(directory))
            with mock.patch.object(
                guardian.os,
                "rename",
                side_effect=OSError("rename refused"),
            ), self.assertRaisesRegex(OSError, "rename refused"):
                guardian._create_lease_generation(active.lease_dir)

            self.assertFalse(active.lease_dir.exists())
            self.assertEqual(
                list(active.lease_dir.parent.glob(f".{active.lease_dir.name}.creating-*")),
                [],
            )

    def test_creation_failure_after_rename_preserves_owned_generation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            active = self.make_guardian(Path(directory))
            real_fsync = os.fsync
            calls = 0

            def fail_parent_fsync(descriptor: int) -> None:
                nonlocal calls
                calls += 1
                if calls == 3:
                    raise OSError("parent fsync failed")
                real_fsync(descriptor)

            with mock.patch.object(
                guardian.os, "fsync", side_effect=fail_parent_fsync
            ), self.assertRaisesRegex(
                guardian.LeaseCreationCommitted, "parent fsync failed"
            ):
                active.acquire()

            self.assertTrue(active.lease_owned)
            self.assertTrue(active.lease_dir.is_dir())
            self.assertIsInstance(active.lease_generation, str)
            active.production_preserved = True
            active.production_identity_verified = True
            active.release()
            self.assertFalse(active.lease_dir.exists())

    def test_abandoned_pretransition_generation_is_reaped_without_receipt(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            active = self.make_guardian(Path(directory))
            _, abandoned_generation = self.create_generation_lease(
                active, phase=guardian.LEASE_PHASE_ACQUIRING
            )

            with mock.patch.object(
                guardian, "_live_guardians_for_lease", return_value=()
            ):
                active.acquire()

            self.assertTrue(active.lease_owned)
            self.assertNotEqual(active.lease_generation, abandoned_generation)
            active.production_preserved = True
            active.production_identity_verified = True
            active.release()

    def test_reconciliation_lock_symlink_and_unsafe_mode_refuse(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            target = root / "target"
            target.write_bytes(b"")
            active.reconciliation_lock.symlink_to(target)
            with self.assertRaisesRegex(guardian.GuardianError, "safely open"):
                active.acquire()
            self.assertFalse(active.lease_dir.exists())

            active.reconciliation_lock.unlink()
            active.reconciliation_lock.write_bytes(b"")
            active.reconciliation_lock.chmod(0o644)
            with self.assertRaisesRegex(guardian.GuardianError, "metadata is unsafe"):
                active.acquire()
            self.assertFalse(active.lease_dir.exists())

    def test_unproven_legacy_lease_is_never_claimed_or_removed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            active = self.make_guardian(Path(directory))
            active.lease_dir.mkdir()
            active.lease_dir.chmod(0o700)
            with mock.patch.object(
                guardian, "_live_guardians_for_lease", return_value=()
            ), self.assertRaisesRegex(
                guardian.GuardianError, "unexpected generation contents"
            ):
                active.acquire()
            self.assertFalse(active.lease_owned)
            self.assertTrue(active.lease_dir.is_dir())

    def test_owner_ended_cleanup_emits_exact_retained_failure(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            active = self.make_guardian(Path(directory))
            active.transition_path = guardian.CORRECTED_TRANSITION
            active.mutation_fence_proved = True
            owner_ended = guardian.OwnerEnded(
                guardian.OWNER_ENDED_BEFORE_COMPLETION
            )
            restore_failure = guardian.GuardianError(
                guardian.PRESERVED_WORKER_OWNERSHIP_FAILURE
            )
            with mock.patch.object(active, "acquire"), mock.patch.object(
                active, "preflight_and_transition"
            ), mock.patch.object(active, "start_candidate"), mock.patch.object(
                active, "wait_for_owner", side_effect=owner_ended
            ), mock.patch.object(active, "stop_candidate"), mock.patch.object(
                active, "finalize_production", side_effect=restore_failure
            ) as restore, mock.patch.object(
                active, "restoration_outstanding", return_value=True
            ), mock.patch.object(guardian.signal, "signal"):
                self.assertEqual(active.run(), 1)

            receipt = json.loads(active.final_receipt.read_text(encoding="utf-8"))
            self.assertEqual(
                receipt["failure"], guardian.RETAINED_OWNER_ENDED_FAILURE
            )
            self.assertEqual(restore.call_count, 2)

    def test_exact_owner_ended_receipt_reconciles_stable_idle_generation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            active = self.make_guardian(Path(directory))
            prior, receipt, _, _, metadata, generation = (
                self.write_retained_owner_ended_evidence(active)
            )
            snapshot = guardian.ProcessSnapshot(
                pid=receipt["old_production_pid"],
                executable=str(active.installed),
                argv=(str(active.installed), "--mode", "shipyard", "daemon", "run"),
                environment={"HOME": str(active.root)},
                cwd=str(active.root),
                stdin_path="/dev/null",
                stdout_path="/dev/null",
                stderr_path="/dev/null",
                start_time=receipt["old_production_start_time"],
            )

            def bind_production(prior_receipt: dict[str, object]):
                self.assertEqual(prior_receipt, receipt)
                active.snapshot = snapshot
                active.installed_hash = receipt["installed_sha256"]
                active.candidate_hash = receipt["candidate_sha256"]
                active.configured_repos = ("owner/repo",)
                active.worker_ids = ()
                active.lock_path = active.root / "writer.lock"
                active.transition_path = guardian.CORRECTED_TRANSITION
                active.old_lifetime_lock_owned = False
                active.mutation_fence_proved = True
                return snapshot, active.configured_repos

            validate_lock_paths = mock.Mock()
            with mock.patch.object(
                guardian, "_live_guardians_for_lease", return_value=()
            ), mock.patch.object(
                guardian, "_pid_alive", return_value=False
            ), mock.patch.object(
                active,
                "snapshot_reconciliation_production",
                side_effect=bind_production,
            ), mock.patch.object(
                active, "verify_reconciliation_production", return_value=()
            ) as verify, mock.patch.object(
                active,
                "final_reconciliation_writer_fence",
                return_value=contextlib.nullcontext(validate_lock_paths),
            ), mock.patch.object(guardian.time, "sleep"), mock.patch.object(
                guardian, "_process_start", return_value=active.owner_start
            ):
                self.assertTrue(active.reconcile_retained_lease())

            self.assertEqual(verify.call_count, 4)
            validate_lock_paths.assert_called_once()
            self.assertFalse(active.lease_dir.exists())
            self.assertFalse(
                active.lease_dir.with_name(
                    f".{active.lease_dir.name}.removed-{generation}"
                ).exists()
            )
            intent = json.loads(
                active.reconciliation_intent.read_text(encoding="utf-8")
            )
            self.assertEqual(intent["prior_canary_root"], str(prior))
            self.assertEqual(intent["lease_device"], metadata.st_dev)
            self.assertEqual(intent["lease_inode"], metadata.st_ino)
            self.assertEqual(intent["lease_ctime_ns"], metadata.st_ctime_ns)
            self.assertEqual(intent["lease_generation"], generation)
            self.assertTrue(intent["mutation_fence_proved"])
            self.assertFalse(active.reconciliation_receipt.exists())

    def test_owner_ended_retained_failure_grammar_is_exact(self) -> None:
        missing_owner_ended = guardian.RETAINED_OWNER_ENDED_FAILURE.replace(
            f"OwnerEnded: {guardian.OWNER_ENDED_BEFORE_COMPLETION}; ", "", 1
        )
        extra_component = (
            f"{guardian.RETAINED_OWNER_ENDED_FAILURE}; "
            "release lease: GuardianError: unrecognized cleanup"
        )
        for name, failure in (
            ("missing OwnerEnded", missing_owner_ended),
            ("extra component", extra_component),
        ):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                active = self.make_guardian(Path(directory))
                _, _, _, _, metadata, generation = (
                    self.write_retained_owner_ended_evidence(
                        active, failure=failure
                    )
                )
                with mock.patch.object(guardian, "_pid_alive", return_value=False):
                    with self.assertRaisesRegex(
                        guardian.GuardianError, "not safely reconcilable"
                    ):
                        active.retained_legacy_evidence()
                current, observed_generation = guardian._validate_lease_generation(
                    active.lease_dir
                )
                self.assertEqual(
                    (current.st_dev, current.st_ino, current.st_ctime_ns),
                    (metadata.st_dev, metadata.st_ino, metadata.st_ctime_ns),
                )
                self.assertEqual(observed_generation, generation)

    def test_owner_ended_retained_receipt_fences_remain_required(self) -> None:
        for drift in (
            "missing mutation proof",
            "generation",
            "production identity",
            "completed reason",
            "production preserved",
            "production identity verified",
        ):
            with self.subTest(drift=drift), tempfile.TemporaryDirectory() as directory:
                active = self.make_guardian(Path(directory))
                prior, receipt, ready, _, metadata, generation = (
                    self.write_retained_owner_ended_evidence(active)
                )
                if drift == "missing mutation proof":
                    receipt["mutation_fence_proved"] = False
                    guardian._atomic_json(prior / "guardian-receipt.json", receipt)
                elif drift == "generation":
                    receipt["lease_generation"] = (
                        "0" * 64 if generation != "0" * 64 else "1" * 64
                    )
                    guardian._atomic_json(prior / "guardian-receipt.json", receipt)
                elif drift == "production identity":
                    ready["production_pid"] = 4243
                    guardian._atomic_json(prior / "ready.json", ready)
                elif drift == "completed reason":
                    receipt["reason"] = "completed"
                    guardian._atomic_json(prior / "guardian-receipt.json", receipt)
                elif drift == "production preserved":
                    receipt["production_preserved"] = True
                    guardian._atomic_json(prior / "guardian-receipt.json", receipt)
                else:
                    receipt["production_identity_verified"] = True
                    guardian._atomic_json(prior / "guardian-receipt.json", receipt)

                with mock.patch.object(guardian, "_pid_alive", return_value=False):
                    with self.assertRaises(guardian.GuardianError):
                        active.retained_legacy_evidence()
                current, observed_generation = guardian._validate_lease_generation(
                    active.lease_dir
                )
                self.assertEqual(
                    (current.st_dev, current.st_ino, current.st_ctime_ns),
                    (metadata.st_dev, metadata.st_ino, metadata.st_ctime_ns),
                )
                self.assertEqual(observed_generation, generation)

    def test_owner_ended_retained_live_candidate_refuses_without_removal(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            active = self.make_guardian(Path(directory))
            _, _, _, _, metadata, generation = (
                self.write_retained_owner_ended_evidence(active)
            )
            with mock.patch.object(guardian, "_pid_alive", return_value=True):
                with self.assertRaisesRegex(guardian.GuardianError, "still alive"):
                    active.retained_legacy_evidence()
            current, observed_generation = guardian._validate_lease_generation(
                active.lease_dir
            )
            self.assertEqual(current.st_ino, metadata.st_ino)
            self.assertEqual(observed_generation, generation)

    def test_owner_ended_retained_live_guardian_refuses_without_removal(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            active = self.make_guardian(Path(directory))
            _, _, _, _, metadata, generation = (
                self.write_retained_owner_ended_evidence(active)
            )
            with mock.patch.object(
                guardian, "_live_guardians_for_lease", return_value=(8442,)
            ):
                with self.assertRaisesRegex(guardian.GuardianError, "live guardian"):
                    active.acquire()
            current, observed_generation = guardian._validate_lease_generation(
                active.lease_dir
            )
            self.assertEqual(current.st_ino, metadata.st_ino)
            self.assertEqual(observed_generation, generation)

    def test_owner_ended_retained_production_drift_refuses_without_removal(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            active = self.make_guardian(Path(directory))
            _, _, _, _, metadata, generation = (
                self.write_retained_owner_ended_evidence(active)
            )
            with mock.patch.object(
                guardian, "_live_guardians_for_lease", return_value=()
            ), mock.patch.object(
                guardian, "_pid_alive", return_value=False
            ), mock.patch.object(
                active,
                "snapshot_reconciliation_production",
                side_effect=guardian.GuardianError(
                    "retained-lease production authority changed: installed_sha256"
                ),
            ):
                with self.assertRaisesRegex(guardian.GuardianError, "authority changed"):
                    active.acquire()
            current, observed_generation = guardian._validate_lease_generation(
                active.lease_dir
            )
            self.assertEqual(current.st_ino, metadata.st_ino)
            self.assertEqual(observed_generation, generation)

    def test_empty_legacy_retained_lease_waits_for_stable_idle_then_reaps(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            active.installed.write_bytes(b"installed")
            active.candidate.write_bytes(b"candidate")
            self.create_generation_lease(active)
            snapshot = guardian.ProcessSnapshot(
                pid=4242,
                executable=str(active.installed),
                argv=(str(active.installed), "--mode", "shipyard", "daemon", "run"),
                environment={"HOME": str(root)},
                cwd=str(root),
                stdin_path="/dev/null",
                stdout_path="/dev/null",
                stderr_path="/dev/null",
                start_time="Sat Aug 29 05:44:36 2026",
            )
            prior_root = root / "shipyard-sandbox-m3-1-1"
            prior = {"old_production_pid": snapshot.pid}

            def bind_snapshot(_prior: dict[str, object]):
                active.snapshot = snapshot
                active.installed_hash = guardian._sha256(active.installed)
                active.candidate_hash = guardian._sha256(active.candidate)
                active.configured_repos = ()
                active.worker_ids = ()
                active.lock_path = root / "writer.lock"
                active.transition_path = guardian.CORRECTED_TRANSITION
                active.old_lifetime_lock_owned = False
                active.mutation_fence_proved = True
                return snapshot, ()

            workers = iter([("sy-live",), (), (), (), ()])
            with mock.patch.object(
                guardian, "_live_guardians_for_lease", return_value=()
            ), mock.patch.object(
                active,
                "retained_legacy_evidence",
                return_value=(prior_root, prior, {}),
            ), mock.patch.object(
                active, "snapshot_reconciliation_production", side_effect=bind_snapshot
            ), mock.patch.object(
                active,
                "verify_reconciliation_production",
                side_effect=lambda: next(workers),
            ) as verify, mock.patch.object(
                active,
                "final_reconciliation_writer_fence",
                return_value=contextlib.nullcontext(mock.Mock()),
            ) as fence, mock.patch.object(
                guardian.time, "sleep"
            ), mock.patch.object(
                guardian, "_process_start", return_value=active.owner_start
            ):
                with self.assertRaises(guardian.ReconciledAfterOwnerEnded):
                    active.acquire()

            self.assertFalse(active.lease_owned)
            self.assertFalse(active.lease_dir.exists())
            self.assertEqual(verify.call_count, 5)
            fence.assert_called_once()
            receipt = json.loads(
                active.reconciliation_receipt.read_text(encoding="utf-8")
            )
            self.assertEqual(receipt["reason"], guardian.RETAINED_RECONCILIATION_REASON)
            self.assertFalse(receipt["lease_removed"])

    def test_retained_lease_worker_reappearance_refuses_without_removal(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            self.create_generation_lease(active)
            inode = active.lease_dir.stat().st_ino
            snapshot = mock.Mock(pid=4242)
            active.snapshot = snapshot
            active.lock_path = root / "writer.lock"
            with mock.patch.object(
                guardian, "_live_guardians_for_lease", return_value=()
            ), mock.patch.object(
                active,
                "retained_legacy_evidence",
                return_value=(root / "prior", {}, {}),
            ), mock.patch.object(
                active,
                "snapshot_reconciliation_production",
                return_value=(snapshot, ()),
            ), mock.patch.object(
                active,
                "verify_reconciliation_production",
                side_effect=[(), (), (), ("sy-new",)],
            ), mock.patch.object(
                active,
                "final_reconciliation_writer_fence",
                return_value=contextlib.nullcontext(mock.Mock()),
            ), mock.patch.object(guardian.time, "sleep"):
                with self.assertRaisesRegex(
                    guardian.GuardianError, "workers reappeared"
                ):
                    active.acquire()
            self.assertEqual(active.lease_dir.stat().st_ino, inode)
            self.assertFalse(active.lease_owned)

    def test_retained_lease_with_live_guardian_refuses_without_removal(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            active = self.make_guardian(Path(directory))
            self.create_generation_lease(active)
            inode = active.lease_dir.stat().st_ino
            with mock.patch.object(
                guardian, "_live_guardians_for_lease", return_value=(7331,)
            ), self.assertRaisesRegex(guardian.GuardianError, "live guardian"):
                active.acquire()
            self.assertEqual(active.lease_dir.stat().st_ino, inode)

    def test_guardian_owner_recognizes_interpreter_and_direct_shebang_argv(self) -> None:
        lease = Path("/tmp/shipyard-sandbox-m3-lease")
        suffix = ("--lease-dir", str(lease), "--owner-pid", "42")
        self.assertTrue(
            guardian._is_guardian_argv_for_lease(
                ("/usr/bin/python3", "/tmp/canary/sandbox-daemon-guardian.py", *suffix),
                lease,
            )
        )
        self.assertTrue(
            guardian._is_guardian_argv_for_lease(
                ("/tmp/canary/sandbox-daemon-guardian.py", *suffix), lease
            )
        )
        self.assertFalse(
            guardian._is_guardian_argv_for_lease(
                ("/tmp/canary/not-the-guardian.py", *suffix), lease
            )
        )

    def test_two_retained_legacy_receipts_refuse_as_ambiguous(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            active.lease_dir = root / "shipyard-sandbox-m3-lease"
            self.create_generation_lease(active)
            for run in ("1-1", "2-1"):
                prior = root / f"shipyard-sandbox-m3-{run}"
                prior.mkdir()
                prior.chmod(0o700)
                receipt = prior / "guardian-receipt.json"
                receipt.write_text(
                    json.dumps({"lease_retained": True}), encoding="utf-8"
                )
                receipt.chmod(0o600)
            with self.assertRaisesRegex(
                guardian.GuardianError, "one unambiguous prior receipt"
            ):
                active.retained_legacy_evidence()

    def test_crash_intent_without_final_receipt_resolves_only_old_inode(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            active.lease_dir = root / "shipyard-sandbox-m3-lease"
            _, generation = self.create_generation_lease(active)
            old = root / "shipyard-sandbox-m3-1-1"
            new = root / "shipyard-sandbox-m3-2-1"
            resolver = root / "shipyard-sandbox-m3-3-1"
            for path in (old, new, resolver):
                path.mkdir(mode=0o700)
            for path in (
                old / "guardian-receipt.json",
                new / "guardian-receipt.json",
                new / "ready.json",
                new / "mutation-fence.json",
                resolver / "retained-reconciliation-intent.json",
            ):
                path.write_text("{}", encoding="utf-8")
            old_receipt = {
                "lease_retained": True,
                "old_production_pid": 111,
                "old_production_start_time": "old",
                "installed_sha256": "a" * 64,
            }
            new_receipt = {
                "schema_version": 1,
                "lease_retained": True,
                "lease_removed": False,
                "transition_path": guardian.CORRECTED_TRANSITION,
                "candidate_stopped": True,
                "production_quiesced": False,
                "production_restored": False,
                "mutation_fence_proved": True,
                "old_lifetime_lock_owned": False,
                "active_runs": [],
                "failure": "active workers changed during idle wait; preserved active worker ownership differs",
                "old_production_pid": 222,
                "old_production_start_time": "new",
                "installed_sha256": "b" * 64,
                "candidate_sha256": "c" * 64,
                "lease_device": active.lease_dir.stat().st_dev,
                "lease_inode": active.lease_dir.stat().st_ino,
                "lease_ctime_ns": active.lease_dir.stat().st_ctime_ns,
                "lease_generation": generation,
            }
            ready = {
                "transition_path": guardian.CORRECTED_TRANSITION,
                "candidate_pid": 333,
                "candidate_sha256": "c" * 64,
                "installed_sha256": "b" * 64,
                "production_pid": 222,
                "production_start_time": "new",
            }
            mutation = {
                "transition_path": guardian.CORRECTED_TRANSITION,
                "returncode": 75,
                "overlap_classification": guardian.WRITER_DOMAIN_OVERLAP_CLASSIFICATION,
                "mutation_absent": True,
                "selected_path_absent": True,
                "production_pid": 222,
                "production_start_time": "new",
            }
            intent = {
                "schema_version": 1,
                "transition_path": guardian.CORRECTED_TRANSITION,
                "mutation_fence_proved": True,
                "prior_canary_root": str(old),
                "lease_device": active.lease_dir.stat().st_dev,
                "lease_inode": active.lease_dir.stat().st_ino + 1,
                "lease_ctime_ns": active.lease_dir.stat().st_ctime_ns,
                "lease_generation": "0" * 64,
                "old_production_pid": 111,
                "old_production_start_time": "old",
                "installed_sha256": "a" * 64,
            }
            objects = {
                active.lease_dir / guardian.LEASE_GENERATION_MARKER: {
                    "schema_version": 1,
                    "generation": generation,
                    "phase": guardian.LEASE_PHASE_TRANSITIONING,
                },
                old / "guardian-receipt.json": old_receipt,
                new / "guardian-receipt.json": new_receipt,
                new / "ready.json": ready,
                new / "mutation-fence.json": mutation,
                resolver / "retained-reconciliation-intent.json": intent,
            }
            with mock.patch.object(
                guardian, "_json_object", side_effect=lambda path: objects[path]
            ), mock.patch.object(guardian, "_pid_alive", return_value=False):
                selected, _, _ = active.retained_legacy_evidence()
            self.assertEqual(selected, new)

    def test_stale_retained_receipt_cannot_authorize_new_lease_generation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            active.lease_dir = root / "shipyard-sandbox-m3-lease"
            _, generation = self.create_generation_lease(active)
            current = active.lease_dir.stat()
            stale_generation = "0" * 64 if generation != "0" * 64 else "1" * 64
            prior = root / "shipyard-sandbox-m3-1-1"
            prior.mkdir(mode=0o700)
            receipt = prior / "guardian-receipt.json"
            receipt.write_text(
                json.dumps(
                    {
                        "lease_retained": True,
                        "lease_device": current.st_dev,
                        "lease_inode": current.st_ino,
                        "lease_ctime_ns": current.st_ctime_ns,
                        "lease_generation": stale_generation,
                    }
                ),
                encoding="utf-8",
            )
            receipt.chmod(0o600)
            with self.assertRaisesRegex(
                guardian.GuardianError, "one unambiguous prior receipt"
            ):
                active.retained_legacy_evidence()
            self.assertEqual(
                (active.lease_dir.stat().st_dev, active.lease_dir.stat().st_ino),
                (current.st_dev, current.st_ino),
            )

    def test_retained_evidence_symlink_and_public_file_refuse(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            real = root / "real.json"
            real.write_text("{}", encoding="utf-8")
            real.chmod(0o600)
            linked = root / "linked.json"
            linked.symlink_to(real)
            with self.assertRaisesRegex(guardian.GuardianError, "unsafe metadata"):
                guardian._json_object(linked)
            real.chmod(0o644)
            with self.assertRaisesRegex(guardian.GuardianError, "unsafe metadata"):
                guardian._json_object(real)

    def test_writer_fence_lock_symlink_refuses(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            real = root / "real.lock"
            real.write_bytes(b"")
            real.chmod(0o600)
            linked = root / "linked.lock"
            linked.symlink_to(real)
            with self.assertRaisesRegex(guardian.GuardianError, "safely open lock"):
                guardian._open_verified_private_lock(linked)

    def test_retained_lease_authority_drift_refuses_without_removal(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            self.create_generation_lease(active)
            inode = active.lease_dir.stat().st_ino
            with mock.patch.object(
                guardian, "_live_guardians_for_lease", return_value=()
            ), mock.patch.object(
                active,
                "retained_legacy_evidence",
                return_value=(root / "prior", {}, {}),
            ), mock.patch.object(
                active,
                "snapshot_reconciliation_production",
                side_effect=guardian.GuardianError(
                    "retained-lease production authority changed: installed_sha256"
                ),
            ):
                with self.assertRaisesRegex(guardian.GuardianError, "authority changed"):
                    active.acquire()
            self.assertEqual(active.lease_dir.stat().st_ino, inode)

    def test_retained_lease_active_timeout_refuses_without_removal(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            self.create_generation_lease(active)
            inode = active.lease_dir.stat().st_ino
            snapshot = mock.Mock(pid=4242)
            active.snapshot = snapshot
            with mock.patch.object(
                guardian, "_live_guardians_for_lease", return_value=()
            ), mock.patch.object(
                active,
                "retained_legacy_evidence",
                return_value=(root / "prior", {}, {}),
            ), mock.patch.object(
                active,
                "snapshot_reconciliation_production",
                return_value=(snapshot, ()),
            ), mock.patch.object(
                guardian, "RETAINED_RECONCILIATION_MAX_SECONDS", 0
            ):
                with self.assertRaisesRegex(guardian.GuardianError, "did not become idle"):
                    active.acquire()
            self.assertEqual(active.lease_dir.stat().st_ino, inode)

    def test_retained_lease_generation_replacement_refuses_new_owner(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            _, old_generation = self.create_generation_lease(active)
            snapshot = mock.Mock(pid=4242)
            active.snapshot = snapshot
            active.lock_path = root / "writer.lock"

            @contextlib.contextmanager
            def replace_lease():
                (active.lease_dir / guardian.LEASE_GENERATION_MARKER).unlink()
                active.lease_dir.rmdir()
                _, replacement_generation = self.create_generation_lease(active)
                self.assertNotEqual(replacement_generation, old_generation)
                yield mock.Mock()

            with mock.patch.object(
                guardian, "_live_guardians_for_lease", return_value=()
            ), mock.patch.object(
                active,
                "retained_legacy_evidence",
                return_value=(root / "prior", {}, {}),
            ), mock.patch.object(
                active,
                "snapshot_reconciliation_production",
                return_value=(snapshot, ()),
            ), mock.patch.object(
                active, "verify_reconciliation_production", return_value=()
            ), mock.patch.object(
                active,
                "final_reconciliation_writer_fence",
                side_effect=replace_lease,
            ), mock.patch.object(guardian.time, "sleep"):
                with self.assertRaisesRegex(guardian.GuardianError, "identity changed"):
                    active.acquire()
            _, observed_generation = guardian._validate_lease_generation(
                active.lease_dir
            )
            self.assertNotEqual(observed_generation, old_generation)
            self.assertFalse(active.lease_owned)

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
            self.assertEqual(len(verified), 5)

    def test_stable_idle_revalidates_after_final_contention_sample(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "writer.lock"
            verify = mock.Mock(
                side_effect=[
                    None,
                    None,
                    None,
                    guardian.GuardianError("final-sample identity drift"),
                ]
            )
            with mock.patch.object(
                guardian, "_lock_holders", return_value=(4242,)
            ), mock.patch.object(
                guardian, "_exclusive_lock_is_contended", return_value=False
            ), mock.patch.object(guardian.time, "sleep"):
                with self.assertRaisesRegex(
                    guardian.GuardianError, "final-sample identity drift"
                ):
                    guardian._wait_for_idle_writer_domain(
                        path, 4242, verify_production=verify
                    )
            self.assertEqual(verify.call_count, 4)

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
            active.acquire()
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
                configured = stack.enter_context(
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
            self.assertEqual(
                configured.call_args.kwargs["state_dir"],
                active.production_pid_file.parent.parent,
            )
            self.assertNotEqual(active.production_state_dir, active.state_dir)
            run.assert_not_called()

    def test_preflight_routes_only_no_holder_contention_through_bounded_wait(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            active.installed.write_bytes(b"installed")
            active.candidate.write_bytes(b"candidate")
            active.production_pid_file.write_text("4242\n", encoding="utf-8")
            active.acquire()
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
                    mock.patch.object(guardian, "_lock_holders", return_value=())
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
                initial_holders=(),
                verify_production=active.verify_unchanged_production,
                diagnostic_root=active.root,
            )
            run.assert_not_called()

    def test_preflight_observes_transient_production_holder_without_stopping(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            active.installed.write_bytes(b"installed")
            active.candidate.write_bytes(b"candidate")
            active.production_pid_file.write_text("4242\n", encoding="utf-8")
            active.acquire()
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
                        guardian, "_lock_holders", return_value=(snapshot.pid,)
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
                stop = stack.enter_context(mock.patch.object(guardian, "_run"))
                active.preflight_and_transition()

            self.assertEqual(active.transition_path, guardian.CORRECTED_TRANSITION)
            self.assertFalse(active.production_quiesced)
            self.assertFalse(active.old_lifetime_lock_owned)
            wait.assert_called_once_with(
                active.lock_path,
                snapshot.pid,
                initial_holders=(snapshot.pid,),
                verify_production=active.verify_unchanged_production,
                diagnostic_root=active.root,
            )
            stop.assert_not_called()

    def test_persistent_production_holder_selects_legacy_after_bounded_wait(self) -> None:
        path = Path("/tmp/writer.lock")
        verify = mock.Mock()
        installed_version = mock.Mock(
            return_value=guardian.LEGACY_LIFETIME_LOCK_VERSION
        )
        retained = guardian.RetainedWriterDomain(
            (4242,),
            continuously_contended=True,
            continuous_production_ownership=True,
        )
        with mock.patch.object(
            guardian, "_wait_for_idle_writer_domain", side_effect=retained
        ), mock.patch.object(
            guardian, "_lock_holders", return_value=(4242,)
        ) as holders, mock.patch.object(
            guardian, "_exclusive_lock_is_contended", return_value=True
        ):
            self.assertEqual(
                guardian._select_transition_after_bounded_observation(
                    path,
                    4242,
                    (4242,),
                    True,
                    verify_production=verify,
                    installed_version=installed_version,
                ),
                guardian.LEGACY_TRANSITION,
            )
        self.assertEqual(verify.call_count, 2)
        self.assertEqual(verify.call_args_list[0], verify.call_args_list[1])
        self.assertIsInstance(verify.call_args.args[0], float)
        shared_deadline = verify.call_args.args[0]
        installed_version.assert_called_once_with(shared_deadline)
        self.assertEqual(holders.call_args.kwargs["deadline"], shared_deadline)

    def test_persistent_then_single_idle_sample_fails_ambiguous(self) -> None:
        retained = guardian.RetainedWriterDomain(
            (4242,),
            continuously_contended=True,
            continuous_production_ownership=True,
        )
        with mock.patch.object(
            guardian, "_wait_for_idle_writer_domain", side_effect=retained
        ), mock.patch.object(
            guardian, "_lock_holders", return_value=(4242,)
        ), mock.patch.object(
            guardian, "_exclusive_lock_is_contended", return_value=False
        ):
            with self.assertRaisesRegex(
                guardian.GuardianError, "ambiguous production writer-domain state"
            ):
                guardian._select_transition_after_bounded_observation(
                    Path("/tmp/writer.lock"),
                    4242,
                    (4242,),
                    True,
                    verify_production=mock.Mock(),
                    installed_version=lambda _deadline: guardian.LEGACY_LIFETIME_LOCK_VERSION,
                )

    def test_legacy_stop_targets_proven_production_state_directory(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            active.installed.write_bytes(b"installed")
            active.candidate.write_bytes(b"candidate")
            active.production_pid_file.write_text("4242\n", encoding="utf-8")
            active.acquire()
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
            stop_result = mock.Mock(returncode=0, stdout="", stderr="")
            with mock.patch.object(
                guardian, "snapshot_process", return_value=snapshot
            ), mock.patch.object(
                guardian, "_configured_repos", return_value=()
            ), mock.patch.object(
                guardian, "_active_runs", return_value=()
            ), mock.patch.object(
                guardian, "_lock_holders", side_effect=[(4242,), ()]
            ), mock.patch.object(
                guardian, "_exclusive_lock_is_contended", side_effect=[True, False]
            ), mock.patch.object(
                guardian,
                "_select_transition_after_bounded_observation",
                return_value=guardian.LEGACY_TRANSITION,
            ), mock.patch.object(
                guardian, "_pid_alive", return_value=False
            ), mock.patch.object(
                guardian, "_run", return_value=stop_result
            ) as run:
                active.preflight_and_transition()

            command = run.call_args.args[0]
            self.assertEqual(
                command,
                [
                    str(active.installed),
                    "--mode",
                    "shipyard",
                    "--state-dir",
                    str(active.production_state_dir),
                    "daemon",
                    "stop",
                ],
            )
            self.assertNotIn(str(active.state_dir), command)
            self.assertEqual(run.call_args.kwargs["cwd"], "/")

    def test_persistent_corrected_version_never_selects_legacy(self) -> None:
        retained = guardian.RetainedWriterDomain(
            (4242,),
            continuously_contended=True,
            continuous_production_ownership=True,
        )
        with mock.patch.object(
            guardian, "_wait_for_idle_writer_domain", side_effect=retained
        ):
            with self.assertRaisesRegex(
                guardian.GuardianError, "not a known legacy lifetime-lock build"
            ):
                guardian._select_transition_after_bounded_observation(
                    Path("/tmp/writer.lock"),
                    4242,
                    (4242,),
                    True,
                    installed_version=lambda _deadline: "0.126.0",
                )

    def test_legacy_gate_uses_running_daemon_version(self) -> None:
        snapshot = guardian.ProcessSnapshot(
            pid=4242,
            executable="/tmp/shipyard",
            argv=("/tmp/shipyard", "daemon", "run"),
            environment={"HOME": "/tmp"},
            cwd="/tmp",
            stdin_path="/dev/null",
            stdout_path="/tmp/daemon.log",
            stderr_path="/tmp/daemon.log",
            start_time="Sat Aug 22 00:00:00 2026",
        )
        with mock.patch.object(
            guardian,
            "_peer_verified_daemon_status",
            return_value={"running": True, "shipyard_version": "0.108.1"},
        ) as status, mock.patch.object(
            guardian, "_installed_cli_version", return_value="0.108.1"
        ) as installed_version:
            self.assertEqual(
                guardian._running_daemon_version(
                    snapshot, Path(snapshot.executable), Path("/tmp/state")
                ),
                "0.108.1",
            )
        status.assert_called_once_with(
            Path("/tmp/state"), snapshot.pid, deadline=None
        )
        installed_version.assert_called_once_with(
            snapshot, Path(snapshot.executable), deadline=None
        )

    def test_active_worker_probe_targets_exact_production_state(self) -> None:
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
        result = mock.Mock(
            stdout=json.dumps({"active_runs": [{"id": "run-b"}, {"id": "run-a"}]})
        )
        with mock.patch.object(guardian, "_run", return_value=result) as run:
            self.assertEqual(
                guardian._active_runs(
                    snapshot,
                    Path(snapshot.executable),
                    state_dir=Path("/proven/production"),
                ),
                ("run-a", "run-b"),
            )
        command = run.call_args.args[0]
        self.assertEqual(
            command,
            [
                snapshot.executable,
                "--cwd",
                "/",
                "--json",
                "--mode",
                "shipyard",
                "--state-dir",
                "/proven/production",
                "status",
            ],
        )
        self.assertNotIn("/candidate/state", command)

    def test_running_legacy_with_newer_disk_image_never_stops_production(self) -> None:
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
            retained = guardian.RetainedWriterDomain(
                (snapshot.pid,),
                continuously_contended=True,
                continuous_production_ownership=True,
            )
            with mock.patch.object(
                guardian, "snapshot_process", return_value=snapshot
            ), mock.patch.object(
                guardian, "_configured_repos", return_value=()
            ), mock.patch.object(
                guardian, "_active_runs", return_value=()
            ), mock.patch.object(
                guardian, "_lock_holders", return_value=(snapshot.pid,)
            ), mock.patch.object(
                guardian, "_exclusive_lock_is_contended", return_value=True
            ), mock.patch.object(
                guardian, "_wait_for_idle_writer_domain", side_effect=retained
            ), mock.patch.object(
                guardian,
                "_peer_verified_daemon_status",
                return_value={
                    "running": True,
                    "shipyard_version": guardian.LEGACY_LIFETIME_LOCK_VERSION,
                },
            ), mock.patch.object(
                guardian, "_installed_cli_version", return_value="0.126.0"
            ), mock.patch.object(guardian, "_run") as stop:
                with self.assertRaisesRegex(
                    guardian.GuardianError, "running and installed.*differ"
                ):
                    active.preflight_and_transition()

            stop.assert_not_called()
            self.assertFalse(active.production_stop_requested)

    def test_peer_verified_status_rejects_foreign_socket_responder(self) -> None:
        connection = mock.MagicMock()
        connection.__enter__.return_value = connection
        connection.getsockopt.return_value = 7331
        with mock.patch.object(
            guardian.socket, "socket", return_value=connection
        ):
            with self.assertRaisesRegex(
                guardian.GuardianError,
                "expected=4242, actual=7331",
            ):
                guardian._peer_verified_daemon_status(
                    Path("/tmp/state"), 4242
                )
        connection.connect.assert_called_once_with("/tmp/state/daemon/daemon.sock")
        connection.sendall.assert_not_called()

    def test_peer_verified_status_accepts_hello_then_status_from_exact_peer(self) -> None:
        connection = mock.MagicMock()
        connection.__enter__.return_value = connection
        connection.getsockopt.return_value = 4242
        connection.recv.return_value = (
            b'{"type":"hello"}\n'
            b'{"type":"status","shipyard_version":"0.108.1"}\n'
        )
        with mock.patch.object(guardian.socket, "socket", return_value=connection):
            self.assertEqual(
                guardian._peer_verified_daemon_status(Path("/tmp/state"), 4242),
                {
                    "type": "status",
                    "shipyard_version": "0.108.1",
                    "running": True,
                },
            )
        connection.sendall.assert_called_once_with(b'{"type":"status"}\n')
        self.assertEqual(connection.getsockopt.call_count, 2)

    def test_peer_verified_status_recomputes_timeout_before_each_frame(self) -> None:
        connection = mock.MagicMock()
        connection.__enter__.return_value = connection
        connection.getsockopt.return_value = 4242
        connection.recv.return_value = b'{"type":"hello"}\n'
        with mock.patch.object(
            guardian.socket, "socket", return_value=connection
        ), mock.patch.object(
            guardian.time, "monotonic", side_effect=[1.0, 2.0, 9.75, 10.1]
        ):
            with self.assertRaisesRegex(
                guardian.GuardianError, "verification deadline expired"
            ):
                guardian._peer_verified_daemon_status(
                    Path("/tmp/state"), 4242, deadline=10.0
                )
        self.assertEqual(
            [call.args[0] for call in connection.settimeout.call_args_list],
            [9.0, 8.0, 0.25],
        )
        connection.recv.assert_called_once_with(65536)

    def test_peer_verified_status_default_budget_rejects_endless_hello(self) -> None:
        connection = mock.MagicMock()
        connection.__enter__.return_value = connection
        connection.getsockopt.return_value = 4242
        connection.recv.return_value = b'{"type":"hello"}\n'
        with mock.patch.object(
            guardian.socket, "socket", return_value=connection
        ), mock.patch.object(
            guardian.time,
            "monotonic",
            side_effect=[0.0, 0.1, 0.2, 14.9, 15.1],
        ):
            with self.assertRaisesRegex(
                guardian.GuardianError, "verification deadline expired"
            ):
                guardian._peer_verified_daemon_status(Path("/tmp/state"), 4242)
        self.assertEqual(connection.settimeout.call_count, 3)
        connection.recv.assert_called_once_with(65536)

    def test_no_holder_sample_never_proves_legacy_ownership(self) -> None:
        retained = guardian.RetainedWriterDomain(
            (4242,),
            continuously_contended=True,
            continuous_production_ownership=False,
        )
        with mock.patch.object(
            guardian, "_wait_for_idle_writer_domain", side_effect=retained
        ):
            with self.assertRaisesRegex(
                guardian.GuardianError, "continuous_production_ownership=False"
            ):
                guardian._select_transition_after_bounded_observation(
                    Path("/tmp/writer.lock"),
                    4242,
                    (),
                    True,
                    installed_version=lambda _deadline: guardian.LEGACY_LIFETIME_LOCK_VERSION,
                )

    def test_intermittent_idle_timeout_never_selects_legacy(self) -> None:
        path = Path("/tmp/writer.lock")
        retained = guardian.RetainedWriterDomain(
            (4242,),
            continuously_contended=False,
            continuous_production_ownership=True,
        )
        with mock.patch.object(
            guardian, "_wait_for_idle_writer_domain", side_effect=retained
        ):
            with self.assertRaisesRegex(
                guardian.GuardianError, "continuously_contended=False"
            ):
                guardian._select_transition_after_bounded_observation(
                    path, 4242, (4242,), True
                )

    def test_bounded_transition_observation_rejects_foreign_or_identity_drift(
        self,
    ) -> None:
        path = Path("/tmp/writer.lock")
        with self.assertRaisesRegex(
            guardian.GuardianError, "ambiguous production writer-domain state"
        ):
            guardian._select_transition_after_bounded_observation(
                path, 4242, (4242, 7331), True
            )

        with mock.patch.object(
            guardian,
            "_wait_for_idle_writer_domain",
            side_effect=guardian.GuardianError("production identity drift"),
        ):
            with self.assertRaisesRegex(guardian.GuardianError, "identity drift"):
                guardian._select_transition_after_bounded_observation(
                    path,
                    4242,
                    (4242,),
                    True,
                    verify_production=mock.Mock(),
                )

    def test_pre_cutover_lifetime_lock_selects_quiesce_restore_path(self) -> None:
        self.assertEqual(
            guardian._select_transition(4242, (4242,), True),
            guardian.LEGACY_TRANSITION,
        )

    def test_restore_retry_adopts_matching_live_daemon_without_second_spawn(self) -> None:
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
            restored = guardian.ProcessSnapshot(
                pid=7331,
                executable=active.snapshot.executable,
                argv=active.snapshot.argv,
                environment=active.snapshot.environment,
                cwd=active.snapshot.cwd,
                stdin_path=active.snapshot.stdin_path,
                stdout_path=active.snapshot.stdout_path,
                stderr_path=active.snapshot.stderr_path,
                start_time="Sat Aug 22 00:05:00 2026",
            )
            with mock.patch.object(
                guardian, "_pid_alive", return_value=True
            ), mock.patch.object(
                guardian, "snapshot_process", return_value=restored
            ), mock.patch.object(
                guardian,
                "_peer_verified_daemon_status",
                return_value={
                    "running": True,
                    "shipyard_version": guardian.LEGACY_LIFETIME_LOCK_VERSION,
                },
            ), mock.patch.object(
                guardian, "_configured_repos", return_value=active.configured_repos
            ), mock.patch.object(
                guardian, "_active_runs", return_value=active.worker_ids
            ), mock.patch.object(
                guardian, "_lock_holders", return_value=(restored.pid,)
            ), mock.patch.object(
                guardian, "_exclusive_lock_is_contended", return_value=True
            ), mock.patch.object(guardian.subprocess, "Popen") as popen:
                active.restore_legacy_production()

            popen.assert_not_called()
            self.assertEqual(active.restored_pid, restored.pid)
            self.assertEqual(active.final_production_start_time, restored.start_time)
            self.assertTrue(active.production_restored)
            self.assertTrue(active.production_identity_verified)

    def test_spawn_path_delegates_directly_to_peer_verified_adoption(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            active.installed.write_bytes(b"installed")
            active.installed_hash = guardian._sha256(active.installed)
            active.snapshot = guardian.ProcessSnapshot(
                pid=4242,
                executable=str(active.installed),
                argv=(str(active.installed), "daemon", "run"),
                environment={"HOME": str(root)},
                cwd=str(root),
                stdin_path="/dev/null",
                stdout_path="/dev/null",
                stderr_path="/dev/null",
                start_time="Sat Aug 22 00:00:00 2026",
            )
            spawned = mock.Mock(pid=7331)
            spawned.poll.return_value = None
            active.production_pid_file.write_text("7331\n", encoding="utf-8")
            with mock.patch.object(
                guardian, "_pid_alive", return_value=False
            ), mock.patch.object(
                guardian.subprocess, "Popen", return_value=spawned
            ), mock.patch.object(
                active, "verify_and_adopt_restored_production"
            ) as adopt, mock.patch.object(
                guardian, "_json_command"
            ) as cli_status, mock.patch.object(
                guardian.time, "monotonic", side_effect=[0.0, 0.0]
            ):
                active.restore_legacy_production()

            adopt.assert_called_once_with(spawned.pid)
            cli_status.assert_not_called()

    def test_restore_adoption_requires_running_ipc_and_exact_stdio(self) -> None:
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
                stdout_path=str(root / "daemon.log"),
                stderr_path=str(root / "daemon.log"),
                start_time="Sat Aug 22 00:00:00 2026",
            )
            restored = guardian.ProcessSnapshot(
                pid=7331,
                executable=active.snapshot.executable,
                argv=active.snapshot.argv,
                environment=active.snapshot.environment,
                cwd=active.snapshot.cwd,
                stdin_path=active.snapshot.stdin_path,
                stdout_path=active.snapshot.stdout_path,
                stderr_path=active.snapshot.stderr_path,
                start_time="Sat Aug 22 00:05:00 2026",
            )
            with mock.patch.object(
                guardian, "snapshot_process", return_value=restored
            ), mock.patch.object(
                guardian, "_peer_verified_daemon_status", return_value={"running": False}
            ), mock.patch.object(
                guardian.time, "monotonic", side_effect=[0.0, 0.0, 16.0]
            ), mock.patch.object(guardian.time, "sleep"):
                with self.assertRaisesRegex(
                    guardian.GuardianError,
                    "never reported running|verification deadline expired",
                ):
                    active.verify_and_adopt_restored_production(restored.pid)

            wrong_stdio = guardian.ProcessSnapshot(
                **{**restored.__dict__, "stdout_path": str(root / "other.log")}
            )
            with mock.patch.object(
                guardian, "snapshot_process", return_value=wrong_stdio
            ):
                with self.assertRaisesRegex(
                    guardian.GuardianError, "stdio identity differs"
                ):
                    active.verify_and_adopt_restored_production(wrong_stdio.pid)

            for reported_version in (None, "0.126.0"):
                with self.subTest(reported_version=reported_version):
                    status = {"running": True}
                    if reported_version is not None:
                        status["shipyard_version"] = reported_version
                    with mock.patch.object(
                        guardian, "snapshot_process", return_value=restored
                    ), mock.patch.object(
                        guardian,
                        "_peer_verified_daemon_status",
                        return_value=status,
                    ):
                        with self.assertRaisesRegex(
                            guardian.GuardianError, "exact legacy version"
                        ):
                            active.verify_and_adopt_restored_production(restored.pid)

            with mock.patch.object(
                guardian, "snapshot_process", return_value=restored
            ), mock.patch.object(
                guardian,
                "_peer_verified_daemon_status",
                return_value={
                    "running": True,
                    "shipyard_version": guardian.LEGACY_LIFETIME_LOCK_VERSION,
                },
            ), mock.patch.object(
                guardian, "_configured_repos", return_value=active.configured_repos
            ), mock.patch.object(
                guardian, "_active_runs", return_value=active.worker_ids
            ), mock.patch.object(
                guardian.time, "monotonic", side_effect=[0.0, 0.0, 0.0, 16.0]
            ):
                with self.assertRaisesRegex(
                    guardian.GuardianError,
                    "stable lifetime-lock ownership|verification deadline expired",
                ):
                    active.verify_and_adopt_restored_production(restored.pid)

            with mock.patch.object(
                guardian, "snapshot_process", return_value=restored
            ), mock.patch.object(
                guardian,
                "_peer_verified_daemon_status",
                return_value={
                    "running": True,
                    "shipyard_version": guardian.LEGACY_LIFETIME_LOCK_VERSION,
                },
            ), mock.patch.object(
                guardian,
                "_configured_repos",
                side_effect=[active.configured_repos, ("other/repo",)],
            ), mock.patch.object(
                guardian, "_active_runs", return_value=active.worker_ids
            ), mock.patch.object(
                guardian, "_lock_holders", return_value=(restored.pid,)
            ), mock.patch.object(
                guardian, "_exclusive_lock_is_contended", return_value=True
            ), mock.patch.object(guardian.time, "sleep"):
                with self.assertRaisesRegex(
                    guardian.GuardianError,
                    "repository authority changed during lock proof",
                ):
                    active.verify_and_adopt_restored_production(restored.pid)

            active.production_pid_file.unlink()
            active.restoration_process = None
            process = mock.Mock(pid=8442)
            process.poll.return_value = None
            with mock.patch.object(
                guardian.subprocess, "Popen", return_value=process
            ) as popen, mock.patch.object(
                guardian.time,
                "monotonic",
                side_effect=[0.0, 16.0, 20.0, 36.0],
            ):
                for _ in range(2):
                    with self.assertRaisesRegex(
                        guardian.GuardianError,
                        "did not own the production pid file",
                    ):
                        active.restore_legacy_production()
            popen.assert_called_once()
            self.assertIs(active.restoration_process, process)

            dead_process = mock.Mock(pid=9553)
            dead_process.poll.return_value = 2
            active.restoration_process = dead_process
            replacement = mock.Mock(pid=9664)
            replacement.poll.return_value = None
            with mock.patch.object(
                guardian.subprocess, "Popen", return_value=replacement
            ) as replacement_spawn, mock.patch.object(
                guardian.time, "monotonic", side_effect=[40.0, 56.0]
            ):
                with self.assertRaisesRegex(
                    guardian.GuardianError, "did not own the production pid file"
                ):
                    active.restore_legacy_production()
            replacement_spawn.assert_called_once()
            self.assertIs(active.restoration_process, replacement)

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

    def test_failed_restore_retains_lease_for_outer_retry(self) -> None:
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
            ["acquire", "quiesce", "start", "wait", "stop", "restore"],
        )

    def test_final_failed_restore_publishes_and_retains_host_lease(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            active = self.make_guardian(Path(directory))
            active.transition_path = guardian.LEGACY_TRANSITION
            active.production_quiesced = True
            active.acquire()
            with mock.patch.object(
                guardian,
                "run_lifecycle",
                side_effect=guardian.GuardianError("initial restore failed"),
            ), mock.patch.object(
                active,
                "finalize_production",
                side_effect=guardian.GuardianError("retry restore failed"),
            ) as restore, mock.patch.object(
                active, "release", wraps=active.release
            ) as release, mock.patch.object(
                guardian.signal, "signal"
            ):
                self.assertEqual(active.run(), 1)

            restore.assert_called_once()
            release.assert_not_called()
            receipt = json.loads(active.final_receipt.read_text(encoding="utf-8"))
            self.assertTrue(receipt["lease_retained"])
            self.assertFalse(receipt["lease_removed"])
            self.assertEqual(receipt["lease_device"], active.lease_device)
            self.assertEqual(receipt["lease_inode"], active.lease_inode)
            self.assertEqual(receipt["lease_ctime_ns"], active.lease_ctime_ns)
            self.assertTrue(active.lease_dir.is_dir())

    def test_partial_quiesce_failure_cannot_release_host_lease(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            active = self.make_guardian(Path(directory))

            def partial_quiesce() -> None:
                active.transition_path = guardian.LEGACY_TRANSITION
                active.production_stop_requested = True
                active.production_quiesced = True
                raise guardian.GuardianError("post-stop holder check failed")

            with self.assertRaisesRegex(
                guardian.GuardianError,
                "release lease.*before production identity is verified",
            ):
                guardian.run_lifecycle(
                    active.acquire,
                    partial_quiesce,
                    mock.Mock(),
                    mock.Mock(),
                    mock.Mock(),
                    mock.Mock(),
                    active.release,
                )

            self.assertTrue(active.lease_owned)
            self.assertTrue(active.lease_dir.is_dir())

    def test_stop_timeout_attempts_outer_restore_and_retains_lease(self) -> None:
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
                guardian, "_lock_holders", return_value=(snapshot.pid,)
            ), mock.patch.object(
                guardian, "_exclusive_lock_is_contended", return_value=True
            ), mock.patch.object(
                guardian,
                "_select_transition_after_bounded_observation",
                return_value=guardian.LEGACY_TRANSITION,
            ), mock.patch.object(
                guardian,
                "_run",
                side_effect=subprocess.TimeoutExpired(["shipyard", "daemon", "stop"], 15),
            ), mock.patch.object(
                active,
                "finalize_production",
                side_effect=guardian.GuardianError("outer restore unresolved"),
            ) as restore, mock.patch.object(guardian.signal, "signal"):
                self.assertEqual(active.run(), 1)

            restore.assert_called_once()
            self.assertTrue(active.production_stop_requested)
            self.assertTrue(active.lease_owned)
            receipt = json.loads(active.final_receipt.read_text(encoding="utf-8"))
            self.assertTrue(receipt["lease_retained"])
            self.assertFalse(receipt["lease_removed"])

    def test_stop_pending_original_daemon_is_never_adopted_as_restored(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            active.installed.write_bytes(b"installed")
            active.installed_hash = guardian._sha256(active.installed)
            active.snapshot = guardian.ProcessSnapshot(
                pid=4242,
                executable=str(active.installed),
                argv=(str(active.installed), "daemon", "run"),
                environment={"HOME": str(root)},
                cwd=str(root),
                stdin_path="/dev/null",
                stdout_path="/dev/null",
                stderr_path="/dev/null",
                start_time="Sat Aug 22 00:00:00 2026",
            )
            active.production_pid_file.write_text("4242\n", encoding="utf-8")
            active.production_stop_requested = True
            with mock.patch.object(
                guardian, "_pid_alive", return_value=True
            ), mock.patch.object(
                guardian, "snapshot_process", return_value=active.snapshot
            ), mock.patch.object(
                guardian.time, "monotonic", side_effect=[0.0, 0.0, 16.0]
            ), mock.patch.object(
                guardian.time, "sleep"
            ), mock.patch.object(
                active, "verify_and_adopt_restored_production"
            ) as adopt:
                with self.assertRaisesRegex(
                    guardian.GuardianError, "still exiting"
                ):
                    active.restore_legacy_production()
            adopt.assert_not_called()

    def test_stop_requested_pid_reuse_with_new_generation_can_be_adopted(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            active.installed.write_bytes(b"installed")
            active.installed_hash = guardian._sha256(active.installed)
            active.snapshot = guardian.ProcessSnapshot(
                pid=4242,
                executable=str(active.installed),
                argv=(str(active.installed), "daemon", "run"),
                environment={"HOME": str(root)},
                cwd=str(root),
                stdin_path="/dev/null",
                stdout_path="/dev/null",
                stderr_path="/dev/null",
                start_time="Sat Aug 22 00:00:00 2026",
            )
            new_generation = guardian.ProcessSnapshot(
                **{
                    **active.snapshot.__dict__,
                    "start_time": "Sat Aug 22 00:01:00 2026",
                }
            )
            active.production_pid_file.write_text("4242\n", encoding="utf-8")
            active.production_stop_requested = True
            with mock.patch.object(
                guardian, "_pid_alive", return_value=True
            ), mock.patch.object(
                guardian, "snapshot_process", return_value=new_generation
            ), mock.patch.object(
                active, "verify_and_adopt_restored_production"
            ) as adopt:
                active.restore_legacy_production()
            adopt.assert_called_once_with(new_generation.pid)

    def test_adoption_reaps_different_retained_restore_child_first(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = self.make_guardian(root)
            active.installed.write_bytes(b"installed")
            active.installed_hash = guardian._sha256(active.installed)
            active.snapshot = guardian.ProcessSnapshot(
                pid=4242,
                executable=str(active.installed),
                argv=(str(active.installed), "daemon", "run"),
                environment={"HOME": str(root)},
                cwd=str(root),
                stdin_path="/dev/null",
                stdout_path="/dev/null",
                stderr_path="/dev/null",
                start_time="Sat Aug 22 00:00:00 2026",
            )
            active.production_pid_file.write_text("7331\n", encoding="utf-8")
            retained = mock.Mock(pid=8442)
            retained.poll.side_effect = [None, 0]
            retained.wait.return_value = 0
            active.restoration_process = retained
            with mock.patch.object(
                guardian, "_pid_alive", side_effect=[True, False]
            ), mock.patch.object(
                active, "verify_and_adopt_restored_production"
            ) as adopt:
                active.restore_legacy_production()

            retained.terminate.assert_called_once()
            retained.wait.assert_called_once_with(timeout=5)
            adopt.assert_called_once_with(7331)
            self.assertIsNone(active.restoration_process)

    def test_untouched_corrected_failure_verifies_then_releases_lease(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            active = self.make_guardian(Path(directory))
            active.transition_path = guardian.CORRECTED_TRANSITION
            active.acquire()

            def prove_untouched(*, require_mutation_fence: bool = True) -> None:
                self.assertFalse(require_mutation_fence)
                active.production_preserved = True
                active.production_identity_verified = True

            with mock.patch.object(
                guardian,
                "run_lifecycle",
                side_effect=guardian.GuardianError("candidate start failed"),
            ), mock.patch.object(
                active,
                "verify_preserved_production",
                side_effect=prove_untouched,
            ) as verify, mock.patch.object(
                active, "release", wraps=active.release
            ) as release, mock.patch.object(guardian.signal, "signal"):
                self.assertEqual(active.run(), 1)

            verify.assert_called_once_with(require_mutation_fence=False)
            release.assert_called_once()
            self.assertFalse(active.lease_owned)
            receipt = json.loads(active.final_receipt.read_text(encoding="utf-8"))
            self.assertFalse(receipt["lease_retained"])
            self.assertTrue(receipt["lease_removed"])

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

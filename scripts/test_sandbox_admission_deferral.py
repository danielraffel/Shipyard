#!/usr/bin/env python3
from __future__ import annotations

import copy
import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path

import sandbox_admission_deferral


HASH = "a" * 64


class SandboxAdmissionDeferralTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.receipt = {
            "schema_version": 1,
            "reason": "failed",
            "failure": sandbox_admission_deferral.ACTIVE_WORKER_FAILURE + " ('sy-1',)",
            "candidate_stopped": True,
            "lease_removed": True,
            "lease_retained": False,
            "mode": "shipyard",
            "active_runs": ["sy-1"],
            "old_production_pid": 123,
            "old_production_start_time": "Sat Aug 29 05:44:36 2026",
            "installed_sha256": HASH,
            "transition_path": None,
            "production_quiesced": False,
            "production_restored": False,
            "production_preserved": False,
            "mutation_fence_proved": False,
            "old_lifetime_lock_owned": False,
            "mutation_probe_output": str(self.root / "unexpected-mutation-ran"),
        }

    def validate(self, receipt: dict[str, object] | None = None) -> dict[str, object]:
        return sandbox_admission_deferral.validate_deferral(
            self.receipt if receipt is None else receipt,
            installed_sha256=HASH,
            canary_root=self.root,
        )

    def test_valid_receipt_emits_exact_durable_marker(self) -> None:
        marker = self.validate()
        self.assertEqual(marker["reason"], "production-queue-active")
        self.assertEqual(marker["old_production_pid"], 123)
        self.assertEqual(marker["mutation_probe_output"], str(self.root / "unexpected-mutation-ran"))

    def test_cli_accepts_valid_receipt(self) -> None:
        receipt_path = self.root / "receipt.json"
        receipt_path.write_text(json.dumps(self.receipt), encoding="utf-8")
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            self.assertEqual(
                sandbox_admission_deferral.main(
                    [
                        "--receipt",
                        str(receipt_path),
                        "--installed-sha256",
                        HASH,
                        "--canary-root",
                        str(self.root),
                    ]
                ),
                0,
            )
        self.assertEqual(json.loads(output.getvalue())["old_production_pid"], 123)

    def test_required_fields_fail_closed(self) -> None:
        for field in self.receipt:
            with self.subTest(field=field):
                invalid = copy.deepcopy(self.receipt)
                del invalid[field]
                with self.assertRaises(sandbox_admission_deferral.DeferralError):
                    self.validate(invalid)

    def test_wrong_pid_and_start_time_refuse(self) -> None:
        for field, value in (
            ("old_production_pid", True),
            ("old_production_pid", 1),
            ("old_production_start_time", ""),
            ("old_production_start_time", 123),
        ):
            with self.subTest(field=field, value=value):
                invalid = copy.deepcopy(self.receipt)
                invalid[field] = value
                with self.assertRaises(sandbox_admission_deferral.DeferralError):
                    self.validate(invalid)

    def test_retained_lease_refuses(self) -> None:
        invalid = copy.deepcopy(self.receipt)
        invalid["lease_retained"] = True
        with self.assertRaises(sandbox_admission_deferral.DeferralError):
            self.validate(invalid)

    def test_wrong_failure_refuses(self) -> None:
        invalid = copy.deepcopy(self.receipt)
        invalid["failure"] = "GuardianError: unrelated"
        with self.assertRaises(sandbox_admission_deferral.DeferralError):
            self.validate(invalid)

    def test_wrong_mutation_probe_path_refuses(self) -> None:
        for value in (None, "null", str(self.root / "another-file")):
            with self.subTest(value=value):
                invalid = copy.deepcopy(self.receipt)
                invalid["mutation_probe_output"] = value
                with self.assertRaises(sandbox_admission_deferral.DeferralError):
                    self.validate(invalid)

    def test_empty_active_runs_and_wrong_hash_refuse(self) -> None:
        invalid = copy.deepcopy(self.receipt)
        invalid["active_runs"] = []
        with self.assertRaises(sandbox_admission_deferral.DeferralError):
            self.validate(invalid)
        with self.assertRaises(sandbox_admission_deferral.DeferralError):
            sandbox_admission_deferral.validate_deferral(
                self.receipt,
                installed_sha256="b" * 64,
                canary_root=self.root,
            )

    def test_retained_reconciliation_emits_exact_durable_marker(self) -> None:
        lease = self.root.parent / "shipyard-sandbox-m3-lease"
        prior = self.root.parent / "shipyard-sandbox-m3-123-1"
        receipt = {
            "schema_version": 1,
            "reason": sandbox_admission_deferral.RETAINED_RECONCILIATION_REASON,
            "guardian_pid": 456,
            "guardian_start_time": "Sat Aug 29 21:00:00 2026",
            "lease_dir": str(lease),
            "lease_device": 42,
            "lease_inode": 789,
            "lease_ctime_ns": 123456789,
            "lease_generation": "c" * 64,
            "prior_canary_root": str(prior),
            "candidate_stopped": True,
            "production_quiesced": False,
            "production_restored": False,
            "transition_path": "corrected-idle-preserve-fence",
            "mutation_fence_proved": True,
            "old_production_pid": 123,
            "old_production_start_time": "Sat Aug 29 05:44:36 2026",
            "installed_sha256": HASH,
            "configured_repos": ["owner/repo"],
            "active_runs": ["sy-live"],
            "lease_removed": False,
        }
        marker = sandbox_admission_deferral.validate_deferral(
            receipt,
            installed_sha256=HASH,
            canary_root=self.root,
            lease_dir=lease,
        )
        self.assertEqual(
            marker["reason"],
            sandbox_admission_deferral.RETAINED_RECONCILIATION_REASON,
        )
        self.assertEqual(marker["lease_inode"], 789)
        self.assertEqual(marker["lease_generation"], "c" * 64)

    def test_retained_reconciliation_refuses_unsafe_state(self) -> None:
        lease = self.root.parent / "shipyard-sandbox-m3-lease"
        prior = self.root.parent / "shipyard-sandbox-m3-123-1"
        base = {
            "schema_version": 1,
            "reason": sandbox_admission_deferral.RETAINED_RECONCILIATION_REASON,
            "guardian_pid": 456,
            "guardian_start_time": "Sat Aug 29 21:00:00 2026",
            "lease_dir": str(lease),
            "lease_device": 42,
            "lease_inode": 789,
            "lease_ctime_ns": 123456789,
            "lease_generation": "c" * 64,
            "prior_canary_root": str(prior),
            "candidate_stopped": True,
            "production_quiesced": False,
            "production_restored": False,
            "transition_path": "corrected-idle-preserve-fence",
            "mutation_fence_proved": True,
            "old_production_pid": 123,
            "old_production_start_time": "Sat Aug 29 05:44:36 2026",
            "installed_sha256": HASH,
            "configured_repos": [],
            "active_runs": ["sy-live"],
            "lease_removed": False,
        }
        for field, value in (
            ("candidate_stopped", False),
            ("production_quiesced", True),
            ("mutation_fence_proved", False),
            ("active_runs", []),
            ("lease_removed", True),
            ("prior_canary_root", str(self.root / "escaped")),
        ):
            with self.subTest(field=field):
                invalid = copy.deepcopy(base)
                invalid[field] = value
                with self.assertRaises(sandbox_admission_deferral.DeferralError):
                    sandbox_admission_deferral.validate_deferral(
                        invalid,
                        installed_sha256=HASH,
                        canary_root=self.root,
                        lease_dir=lease,
                    )

    def make_live_retained_lease(self) -> tuple[Path, dict[str, object]]:
        lease = self.root / "shipyard-sandbox-m3-lease"
        lease.mkdir(mode=0o700)
        generation = "c" * 64
        generation_path = lease / sandbox_admission_deferral.LEASE_GENERATION_MARKER
        generation_path.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "generation": generation,
                    "phase": "transitioning",
                }
            ),
            encoding="utf-8",
        )
        generation_path.chmod(0o600)
        identity = lease.stat()
        marker = {
            "reason": sandbox_admission_deferral.RETAINED_RECONCILIATION_REASON,
            "lease_device": identity.st_dev,
            "lease_inode": identity.st_ino,
            "lease_ctime_ns": identity.st_ctime_ns,
            "lease_generation": generation,
        }
        return lease, marker

    def test_live_retained_lease_requires_complete_exact_identity(self) -> None:
        lease, marker = self.make_live_retained_lease()
        self.assertEqual(
            sandbox_admission_deferral.validate_live_retained_lease(
                marker, lease_dir=lease
            ),
            marker,
        )
        for field in (
            "lease_device",
            "lease_inode",
            "lease_ctime_ns",
            "lease_generation",
        ):
            with self.subTest(field=field):
                changed = copy.deepcopy(marker)
                changed[field] = (
                    "d" * 64 if field == "lease_generation" else int(changed[field]) + 1
                )
                with self.assertRaises(sandbox_admission_deferral.DeferralError):
                    sandbox_admission_deferral.validate_live_retained_lease(
                        changed, lease_dir=lease
                    )

    def test_live_retained_lease_requires_schema_phase_and_sole_marker(self) -> None:
        lease, marker = self.make_live_retained_lease()
        generation_path = lease / sandbox_admission_deferral.LEASE_GENERATION_MARKER
        for payload in (
            {"schema_version": 2, "generation": "c" * 64, "phase": "transitioning"},
            {"schema_version": 1, "generation": "c" * 64, "phase": "acquiring"},
        ):
            with self.subTest(payload=payload):
                generation_path.write_text(json.dumps(payload), encoding="utf-8")
                generation_path.chmod(0o600)
                current = lease.stat()
                changed = copy.deepcopy(marker)
                changed["lease_ctime_ns"] = current.st_ctime_ns
                with self.assertRaises(sandbox_admission_deferral.DeferralError):
                    sandbox_admission_deferral.validate_live_retained_lease(
                        changed, lease_dir=lease
                    )
        generation_path.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "generation": "c" * 64,
                    "phase": "transitioning",
                }
            ),
            encoding="utf-8",
        )
        generation_path.chmod(0o600)
        (lease / "foreign").write_text("unexpected", encoding="utf-8")
        with self.assertRaises(sandbox_admission_deferral.DeferralError):
            sandbox_admission_deferral.validate_live_retained_lease(
                marker, lease_dir=lease
            )

    def test_workflow_preserves_live_retained_lease_reconciler(self) -> None:
        workflow = (Path(__file__).parent.parent / ".github/workflows/sandbox-e2e.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn('"$canary_root/retained-reconciliation.json"', workflow)
        self.assertIn("retained-lease-awaiting-idle)", workflow)
        retained_case = workflow.split("retained-lease-awaiting-idle)", 1)[1].split(
            ";;", 1
        )[0]
        self.assertIn("retained_state_ok=false", retained_case)
        self.assertGreaterEqual(
            retained_case.count("--verify-live-marker"), 2
        )
        self.assertIn('kill -0 "$guardian_pid"', retained_case)
        self.assertNotIn("launchctl bootout", retained_case)
        self.assertIn("this is not physical-canary or release acceptance", workflow)
        self.assertIn("rerun only the targeted macOS Sandbox job", workflow)


if __name__ == "__main__":
    unittest.main()

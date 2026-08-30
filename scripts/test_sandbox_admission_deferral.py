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


if __name__ == "__main__":
    unittest.main()

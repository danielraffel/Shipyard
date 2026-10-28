#!/usr/bin/env python3
import subprocess
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from autonomy_contract_check import _validate_test_reference, validate_contract

ROOT = Path(__file__).resolve().parents[1]

class AutonomyContractTest(unittest.TestCase):
    def test_contract_check_passes(self):
        result = subprocess.run(
            [sys.executable, str(ROOT / "scripts" / "autonomy_contract_check.py")],
            cwd=ROOT, text=True, capture_output=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr or result.stdout)
        self.assertIn("A1-A12", result.stdout)
        self.assertIn("F1-F8", result.stdout)
        self.assertIn("enforcement surfaces", result.stdout)

    def test_checker_rejects_unknown_evidence_test(self):
        text = (ROOT / "docs" / "autonomy-contract.md").read_text(encoding="utf-8")
        broken = text.replace(
            "test: src/immutable_store.rs::immutable_replay_survives_reopen_and_conflicts_fail",
            "test: src/immutable_store.rs::missing_evidence_test",
            1,
        )
        with self.assertRaises(SystemExit):
            validate_contract(broken, ROOT)

    def test_checker_rejects_untyped_evidence_prose(self):
        text = (ROOT / "docs" / "autonomy-contract.md").read_text(encoding="utf-8")
        broken = text.replace(
            "gap: recovery-worker lease integration receipt",
            "recovery-worker lease tests",
            1,
        )
        with self.assertRaises(SystemExit):
            validate_contract(broken, ROOT)

    def test_checker_rejects_missing_blocked_state_contract(self):
        text = (ROOT / "docs" / "autonomy-contract.md").read_text(encoding="utf-8")
        start = text.index("## Blocked-state invariants")
        end = text.index("## Failure and liveness matrix", start)
        broken = text[:start] + text[end:]
        with self.assertRaises(SystemExit):
            validate_contract(broken, ROOT)

    def test_checker_rejects_missing_blocked_state_holds(self):
        text = (ROOT / "docs" / "autonomy-contract.md").read_text(encoding="utf-8")
        broken = text.replace(
            "- missing, stale, contradictory, or unauthenticated provider evidence; and\n- uncertain worker liveness, request identity, or host ownership.\n",
            "",
            1,
        )
        with self.assertRaises(SystemExit):
            validate_contract(broken, ROOT)

    def test_checker_rejects_helper_named_like_a_test(self):
        text = (ROOT / "docs" / "autonomy-contract.md").read_text(encoding="utf-8")
        broken = text.replace(
            "test: src/immutable_store.rs::immutable_replay_survives_reopen_and_conflicts_fail",
            "test: src/immutable_store.rs::write_json_atomic",
            1,
        )
        with self.assertRaises(SystemExit):
            validate_contract(broken, ROOT)

    def test_checker_rejects_ignored_test_reference(self):
        source = "#[test]\n#[ignore]\nfn immutable_replay_survives_reopen_and_conflicts_fail() {}"
        with self.assertRaises(SystemExit):
            _validate_test_reference(
                "A1", "src/immutable_store.rs",
                "immutable_replay_survives_reopen_and_conflicts_fail", source,
            )

if __name__ == "__main__":
    unittest.main()

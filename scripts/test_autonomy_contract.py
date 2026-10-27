#!/usr/bin/env python3
import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

class AutonomyContractTest(unittest.TestCase):
    def test_contract_check_passes(self):
        result = subprocess.run(
            [sys.executable, str(ROOT / "scripts" / "autonomy_contract_check.py")],
            cwd=ROOT, text=True, capture_output=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr or result.stdout)
        self.assertIn("A1-A12", result.stdout)
        self.assertIn("enforcement surfaces", result.stdout)

if __name__ == "__main__":
    unittest.main()

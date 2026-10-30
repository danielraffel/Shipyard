#!/usr/bin/env python3
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent
SCRIPT = ROOT / "run_baseline_collector.py"


class WrapperTests(unittest.TestCase):
    def _run(self, check_exit: int, observations=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            handoff = root / "handoff.py"
            collector = root / "collector.py"
            args_file = root / "args.json"
            handoff_called = root / "handoff-called"
            env = os.environ.copy()
            env.pop("SHIPYARD_OBSERVATIONS", None)
            if observations is not None:
                env["SHIPYARD_OBSERVATIONS"] = observations
            handoff.write_text(
                "import pathlib,sys; pathlib.Path(%r).touch(); sys.exit(%d if sys.argv[1:2] == ['check'] else 0)\n"
                % (str(handoff_called), check_exit)
            )
            collector.write_text(
                "import json,sys; json.dump(sys.argv[1:], open(%r, 'w'))\n" % str(args_file)
            )
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "--collector", str(collector),
                 "--handoff-script", str(handoff), "--repo", "owner/repo",
                 "--merge-receipts", str(root / "receipts.jsonl"),
                 "--merge-receipt-health", str(root / "receipts.health.json"),
                 "--", "--db", "metrics.db"], check=False, env=env, capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 0)
            if observations is not None and observations.strip().lower() == "off":
                self.assertFalse(handoff_called.exists())
                self.assertFalse(args_file.exists())
                self.assertIn("collection disabled", result.stderr)
                return None
            self.assertTrue(handoff_called.exists())
            return json.loads(args_file.read_text())

    def test_optional_observations_off_skips_handoff_and_collector(self):
        for value in ["off", "  OfF\t"]:
            with self.subTest(value=value):
                self.assertIsNone(self._run(0, observations=value))

    def test_invalid_handoff_collects_without_receipt_import(self):
        args = self._run(3)
        self.assertEqual(args[:2], ["--db", "metrics.db"])
        self.assertIn("--merge-receipt-unavailable-reason", args)
        self.assertNotIn("--merge-receipts", args)

    def test_fresh_handoff_adds_receipt_import_flags(self):
        args = self._run(0)
        self.assertIn("--merge-receipts", args)
        self.assertIn("--merge-receipt-health", args)
        self.assertNotIn("--merge-receipt-unavailable-reason", args)


if __name__ == "__main__":
    unittest.main()

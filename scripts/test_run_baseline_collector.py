#!/usr/bin/env python3
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent
SCRIPT = ROOT / "run_baseline_collector.py"


class WrapperTests(unittest.TestCase):
    def _run(self, check_exit: int):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            handoff = root / "handoff.py"
            collector = root / "collector.py"
            args_file = root / "args.json"
            handoff.write_text(
                "import sys; sys.exit(%d if sys.argv[1:2] == ['check'] else 0)\n" % check_exit
            )
            collector.write_text(
                "import json,sys; json.dump(sys.argv[1:], open(%r, 'w'))\n" % str(args_file)
            )
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "--collector", str(collector),
                 "--handoff-script", str(handoff), "--repo", "owner/repo",
                 "--merge-receipts", str(root / "receipts.jsonl"),
                 "--merge-receipt-health", str(root / "receipts.health.json"),
                 "--merge-receipt-source-host", "m3",
                 "--", "--db", "metrics.db"], check=False,
            )
            self.assertEqual(result.returncode, 0)
            return json.loads(args_file.read_text())

    def test_invalid_handoff_collects_without_receipt_import(self):
        args = self._run(3)
        self.assertEqual(args[:2], ["--db", "metrics.db"])
        self.assertIn("--merge-receipt-unavailable-reason", args)
        self.assertNotIn("--merge-receipts", args)

    def test_fresh_handoff_adds_receipt_import_flags(self):
        args = self._run(0)
        self.assertIn("--merge-receipts", args)
        self.assertIn("--merge-receipt-health", args)
        self.assertIn("--merge-receipt-source-host", args)
        self.assertIn("m3", args)
        self.assertNotIn("--merge-receipt-unavailable-reason", args)


if __name__ == "__main__":
    unittest.main()

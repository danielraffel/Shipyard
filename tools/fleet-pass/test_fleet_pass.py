"""Integration controls for the scheduled fleet-pass runner."""
import os
import pathlib
import subprocess
import tempfile
import unittest


HERE = pathlib.Path(__file__).resolve().parent


class FleetPassRunner(unittest.TestCase):
    def test_selftest_exercises_two_slot_shared_claims(self):
        # Keep every temporary file under the repository worktree.
        with tempfile.TemporaryDirectory(dir=HERE.parent.parent) as scratch:
            env = os.environ.copy()
            env["TMPDIR"] = scratch
            result = subprocess.run(
                [str(HERE / "fleet-pass.sh"), "--selftest"],
                cwd=HERE.parent.parent,
                env=env,
                capture_output=True,
                text=True,
                timeout=30,
            )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("PASS queue-claims-exclusive", result.stdout)
        self.assertIn("SELFTEST OK", result.stdout)


if __name__ == "__main__":
    unittest.main()

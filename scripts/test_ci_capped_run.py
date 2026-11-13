"""Tests for scripts/ci-capped-run.sh."""

import shutil
import subprocess
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("ci-capped-run.sh")

# Prints the job caps it sees, so a test can assert them.
SHOW = ["bash", "-c", 'echo "jobs=$CARGO_BUILD_JOBS threads=$RUST_TEST_THREADS"; exit ${EXIT_WITH:-0}']


def run(env, cmd=SHOW):
    base = {"PATH": "/usr/bin:/bin:/usr/sbin"}
    return subprocess.run(["bash", str(SCRIPT), *cmd], env={**base, **env},
                          text=True, capture_output=True, timeout=30)


class CiCappedRunTests(unittest.TestCase):
    def test_github_hosted_runs_unchanged(self):
        result = run({"RUNNER_ENVIRONMENT": "github-hosted"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("jobs= threads=", result.stdout)

    def test_self_hosted_is_capped_at_two(self):
        result = run({"RUNNER_ENVIRONMENT": "self-hosted"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("jobs=2 threads=2", result.stdout)

    def test_override_and_bad_values(self):
        self.assertIn("jobs=3", run({"RUNNER_ENVIRONMENT": "self-hosted", "SHIPYARD_CI_JOBS": "3"}).stdout)
        for bad in ("0", "abc", ""):
            out = run({"RUNNER_ENVIRONMENT": "self-hosted", "SHIPYARD_CI_JOBS": bad}).stdout
            self.assertIn("jobs=2", out, bad)

    def test_exit_status_propagates(self):
        result = run({"RUNNER_ENVIRONMENT": "self-hosted", "EXIT_WITH": "7"})
        self.assertEqual(result.returncode, 7)

    @unittest.skipUnless(shutil.which("taskpolicy"), "macOS only")
    def test_runs_under_utility_qos_on_macos(self):
        result = run({"RUNNER_ENVIRONMENT": "self-hosted"}, cmd=["true"])
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()

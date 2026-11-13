"""Tests for scripts/ci-leased-run.sh with a fake tartci."""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("ci-leased-run.sh")

FAKE_TARTCI = """#!/bin/bash
echo "$*" >> "$FAKE_LOG"
case "$2" in
  acquire)
    case " $* " in *" --class "*) [ -n "$FAKE_OLD" ] && exit 2 ;; esac
    if [ -n "$FAKE_DENY" ]; then echo '{"ok": false, "reason": "busy"}'; exit 75; fi
    echo "{\\"ok\\": true, \\"lease_size_cores\\": ${FAKE_SIZE:-3}}" ;;
  *) echo '{"ok": true}' ;;
esac
"""

# Prints the job caps it sees, so a test can assert them.
SHOW = ["bash", "-c", 'echo "jobs=$CARGO_BUILD_JOBS threads=$RUST_TEST_THREADS"; exit ${EXIT_WITH:-0}']


class CiLeasedRunTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.log = root / "calls.log"
        self.tartci = root / "tartci"
        self.tartci.write_text(FAKE_TARTCI)
        self.tartci.chmod(0o755)
        self.env = {
            "PATH": "/usr/bin:/bin:/usr/sbin",
            "HOME": str(root),
            "RUNNER_ENVIRONMENT": "self-hosted",
            "SHIPYARD_TARTCI_BIN": str(self.tartci),
            "FAKE_LOG": str(self.log),
            "GITHUB_RUN_ID": "42",
            "SHIPYARD_CI_LEASE_WAIT_SECS": "1",
        }

    def tearDown(self):
        self.tmp.cleanup()

    def run_wrapper(self, cmd=None, **env):
        merged = {**self.env, **env}
        merged = {k: v for k, v in merged.items() if v is not None}
        return subprocess.run(["bash", str(SCRIPT), *(cmd or SHOW)], env=merged,
                              text=True, capture_output=True, timeout=30)

    def calls(self):
        return self.log.read_text().splitlines() if self.log.exists() else []

    def test_github_hosted_runs_unchanged(self):
        result = self.run_wrapper(RUNNER_ENVIRONMENT="github-hosted")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("jobs= threads=", result.stdout)
        self.assertEqual(self.calls(), [])

    def test_granted_lease_caps_jobs_and_is_released(self):
        result = self.run_wrapper()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("jobs=3 threads=3", result.stdout)
        calls = self.calls()
        self.assertTrue(calls[0].startswith("leases acquire"))
        self.assertIn("--class background", calls[0])
        self.assertIn("--job-id 42", calls[0])
        self.assertTrue(calls[-1].startswith("leases release"))

    def test_denied_lease_runs_at_the_floor(self):
        result = self.run_wrapper(FAKE_DENY="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("jobs=2 threads=2", result.stdout)
        self.assertIn("no lease granted", result.stderr)

    def test_old_tartci_retries_without_class(self):
        result = self.run_wrapper(FAKE_OLD="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        acquires = [c for c in self.calls() if c.startswith("leases acquire")]
        self.assertEqual(len(acquires), 2)
        self.assertNotIn("--class", acquires[1])
        self.assertIn("jobs=3", result.stdout)

    def test_oversized_grant_is_handed_back(self):
        result = self.run_wrapper(FAKE_SIZE="64")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("jobs=2 threads=2", result.stdout)
        self.assertTrue(any(c.startswith("leases release") for c in self.calls()))

    def test_missing_tartci_runs_at_the_floor(self):
        result = self.run_wrapper(SHIPYARD_TARTCI_BIN="/nonexistent/tartci")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("jobs=2 threads=2", result.stdout)
        self.assertIn("tartci not found", result.stderr)

    def test_command_failure_propagates_and_still_releases(self):
        result = self.run_wrapper(EXIT_WITH="7")
        self.assertEqual(result.returncode, 7)
        self.assertTrue(self.calls()[-1].startswith("leases release"))

    def test_heartbeat_refreshes_a_long_lease(self):
        result = self.run_wrapper(cmd=["sleep", "3"], SHIPYARD_CI_LEASE_HEARTBEAT_SECS="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(any(c.startswith("leases heartbeat") for c in self.calls()))


if __name__ == "__main__":
    unittest.main()

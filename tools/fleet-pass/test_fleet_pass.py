"""Integration checks for the shell runner's repair-budget wiring."""
import contextlib
import importlib.util
import io
import json
import os
import pathlib
import subprocess
import tempfile
import time
import unittest


HERE = os.path.dirname(os.path.abspath(__file__))
HERE_PATH = pathlib.Path(HERE)
RUNNER = os.path.join(HERE, "fleet-pass.sh")
# None falls back to the platform temp dir, which exists on every runner.
SCRATCH_ROOT = os.environ.get("FLEET_PASS_TEST_TMP") or None
SHA = "b" * 40
LINE = f"danielraffel/Shipyard#1225 {SHA} HOLD"


class RepairRunner(unittest.TestCase):
    def test_unchanged_line_is_held_and_reaches_operator_feed(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH_ROOT) as root:
            home = os.path.join(root, "home")
            state = os.path.join(root, "state")
            logs = os.path.join(root, "logs")
            brief = os.path.join(root, "brief")
            os.makedirs(os.path.join(home, ".local", "bin"))
            os.makedirs(os.path.join(home, ".config", "shipyard", "guards"))
            os.makedirs(state)
            os.makedirs(brief)
            with open(os.path.join(home, ".config", "shipyard", "guards", "reviewer-signers.json"), "w") as f:
                json.dump({"default": []}, f)
            with open(os.path.join(brief, "2026-10-31-standing-repair-brief.md"), "w") as f:
                f.write("repair test\n")
            with open(os.path.join(state, "repair_budget.py"), "w") as dst, open(
                    os.path.join(HERE, "repair_budget.py")) as src:
                dst.write(src.read())
            with open(os.path.join(state, "must-review.py"), "w") as f:
                f.write(f"print({LINE!r})\n")
            attempts = os.path.join(state, "repair-attempts.log")
            now = int(time.time())
            with open(attempts, "w") as f:
                for _ in range(3):
                    f.write(f"{now}\t{LINE}\n")
            sr_log = os.path.join(root, "sr.log")
            with open(os.path.join(home, ".local", "bin", "sr"), "w") as f:
                f.write("""#!/bin/sh
if [ "$1" = status ]; then
  printf '%s\\n' '{"providers":{"claude":{"accounts":[{"label":"fake","weekly_left_pct":100,"session_left_pct":100,"state":"ready"}]},"codex":{"accounts":[]}}}'
else
  echo exec >> "$SR_LOG"
fi
""")
            os.chmod(os.path.join(home, ".local", "bin", "sr"), 0o755)
            env = {
                **os.environ,
                "HOME": home,
                "FLEET_PASS_STATE": state,
                "FLEET_PASS_LOGDIR": logs,
                "FLEET_PASS_BRIEF_DIR": brief,
                "FLEET_PASS_MIN_WEEKLY_PCT": "0",
                "FLEET_PASS_POLL_SECONDS": "1",
                "FLEET_PASS_IDLE_SECONDS": "2",
                "FLEET_PASS_CEILING_SECONDS": "5",
                "SR_LOG": sr_log,
            }
            run = subprocess.run([RUNNER, "repair"], env=env, capture_output=True, text=True)
            self.assertEqual(run.returncode, 0, run.stderr)
            with open(os.path.join(state, "repair.must-review")) as f:
                self.assertEqual(f.read(), "")
            with open(os.path.join(state, "repair.held")) as f:
                held = f.read()
            self.assertIn("HELD " + LINE + " attempts=3", held)
            self.assertFalse(os.path.exists(sr_log), "held queue must not start the model")

            spec = importlib.util.spec_from_file_location(
                "operator_findings", os.path.join(HERE, "operator-findings.py"))
            operator = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(operator)
            operator.mr.REPOS = []
            old_state = os.environ.get("FLEET_PASS_STATE")
            os.environ["FLEET_PASS_STATE"] = state
            output = io.StringIO()
            try:
                with contextlib.redirect_stdout(output):
                    with self.assertRaises(SystemExit) as done:
                        operator.main()
                self.assertEqual(done.exception.code, 0)
            finally:
                if old_state is None:
                    os.environ.pop("FLEET_PASS_STATE", None)
                else:
                    os.environ["FLEET_PASS_STATE"] = old_state
            self.assertIn(
                f"danielraffel/Shipyard#1225 {SHA} REPAIR-BUDGET-HELD attempts=3 reason=HOLD",
                output.getvalue(),
            )



class FleetPassSelftest(unittest.TestCase):
    def test_selftest_exercises_two_slot_shared_claims(self):
        # Keep every temporary file under the repository worktree.
        with tempfile.TemporaryDirectory(dir=HERE_PATH.parent.parent) as scratch:
            env = os.environ.copy()
            env["TMPDIR"] = scratch
            result = subprocess.run(
                [str(HERE_PATH / "fleet-pass.sh"), "--selftest"],
                cwd=HERE_PATH.parent.parent,
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

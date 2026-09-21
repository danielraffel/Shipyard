#!/usr/bin/env python3
"""A gate that COULD NOT RUN must fail, never pass.

Two defects met here. The gate scripts are shared with Pulp, whose config lives
at ``tools/scripts/versioning.json``; Shipyard keeps its own at
``scripts/versioning.json``. The scripts hard-coded Pulp's path, so a bare
invocation in THIS repo never found its config. It exited 2 correctly — and the
pre-push hook then threw that away: ``*) echo "…: internal error"`` printed a
line and continued WITHOUT setting ``fail``. A gate that checked nothing
reported success, which is how a missing version bump rides to main with no tag
and no release.

So both halves are pinned: the config now resolves this repo's own layout (and
names every path it searched when it genuinely cannot find one), and an
unmeasurable exit code blocks instead of passing.
"""

from __future__ import annotations

import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PREPUSH = ROOT / ".githooks" / "pre-push"
SCRIPTS = ROOT / "scripts"


class ConfigResolutionTest(unittest.TestCase):
    """A bare invocation must find this repo's config, or say what it searched."""

    def _run(self, script: str, *extra: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, "-B", str(SCRIPTS / script),
             "--mode=report", "--base", "origin/main", *extra],
            cwd=ROOT, capture_output=True, text=True,
        )

    def test_bare_invocation_finds_this_repos_config(self) -> None:
        """The reported defect: no --config meant 'config not found', always."""
        for script in ("version_bump_check.py", "skill_sync_check.py"):
            with self.subTest(script=script):
                proc = self._run(script)
                combined = proc.stdout + proc.stderr
                self.assertNotIn("config not found", combined)
                self.assertNotIn("CANNOT MEASURE", combined)
                self.assertEqual(proc.returncode, 0, combined)

    def test_missing_config_fails_closed_naming_the_paths(self) -> None:
        for script in ("version_bump_check.py", "skill_sync_check.py"):
            with self.subTest(script=script):
                proc = self._run(script, "--config", "/nonexistent/versioning.json")
                self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
                self.assertIn("CANNOT MEASURE", proc.stderr)
                self.assertIn("/nonexistent/versioning.json", proc.stderr)
                self.assertIn("not a pass", proc.stderr)


class PrePushBlocksUnmeasurableGateTest(unittest.TestCase):
    def test_no_gate_falls_open(self) -> None:
        source = PREPUSH.read_text(encoding="utf-8")
        blocking = re.findall(
            r'^\s*\*\) gate_could_not_run "([\w-]+)" "\$gate_rc"; fail=1 ;;$',
            source, re.M,
        )
        fall_open = re.findall(
            r'^\s*\*\) echo "\[pre-push\] ([\w-]+): internal error', source, re.M
        )
        # Control first, on the TOTAL: a regex that silently stopped matching
        # would report "nothing falls open" over zero coverage. Counting both
        # halves means a gate moving between them cannot fake the control.
        self.assertGreaterEqual(
            len(blocking) + len(fall_open), 3,
            f"scan found only {len(blocking) + len(fall_open)} gate branches; "
            "the pattern has drifted from the hook and is measuring nothing",
        )
        self.assertEqual(
            fall_open, [],
            f"these gates still fall OPEN on a 'could not run' exit code "
            f"(they print and continue without setting fail=1): {fall_open}",
        )
        self.assertEqual(
            source.count("gate_rc=$?; case $gate_rc in"), len(blocking),
            'a branch is reading a stale "$gate_rc"',
        )

    def test_helper_blocks_on_every_unmeasurable_code(self) -> None:
        for gate_exit, expect_block in ((0, False), (1, True), (2, True), (127, True)):
            with self.subTest(gate_exit=gate_exit):
                with tempfile.TemporaryDirectory() as td:
                    script = Path(td) / "harness.sh"
                    script.write_text(
                        # Source only the helper definition out of the hook.
                        f'eval "$(sed -n \'/^gate_could_not_run()/,/^}}/p\' '
                        f'"{PREPUSH}")"\n'
                        "fail=0\n"
                        f"( exit {gate_exit} )\n"
                        "gate_rc=$?; case $gate_rc in\n"
                        "    0) ;;\n"
                        "    1) fail=1 ;;\n"
                        '    *) gate_could_not_run "stub" "$gate_rc"; fail=1 ;;\n'
                        "esac\n"
                        'exit "$fail"\n',
                        encoding="utf-8",
                    )
                    proc = subprocess.run(
                        ["bash", str(script)], capture_output=True, text=True
                    )
                self.assertEqual(
                    proc.returncode != 0, expect_block,
                    f"exit {gate_exit}: {proc.stdout + proc.stderr}",
                )
                if gate_exit not in (0, 1):
                    self.assertIn("COULD NOT RUN", proc.stderr)

    def test_pre_versioning_skip_is_loud(self) -> None:
        """The documented no-op is preserved, but can't be read as a pass."""
        source = PREPUSH.read_text(encoding="utf-8")
        self.assertIn("SKIPPED", source)
        self.assertIn("This is a SKIP, not a pass.", source)


if __name__ == "__main__":
    unittest.main()

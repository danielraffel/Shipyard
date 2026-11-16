#!/usr/bin/env python3
"""The pre-push hook's claim-fence line: a refusal stops the push, anything
else the fence cannot answer lets it through with a note."""
from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

from test_prepush_rust_gates import HOOK, ROOT, provision

REFS = "refs/heads/fix 1111 refs/heads/fix 2222\n"
URL = "git@github.com:Generous-Corp/vellum.git"


def run_with_fence(root: Path, status: int) -> tuple[subprocess.CompletedProcess[str], str]:
    env = os.environ.copy()
    env.pop("RUSTUP_AUTO_INSTALL", None)
    env.update(provision(root))
    env["SHIPYARD_PREPUSH_BASE"] = "HEAD"
    env["SHIPYARD_ENFORCE_PREPUSH"] = "0"
    seen = root / "fence-call.txt"
    fence = Path(env["PATH"].split(os.pathsep)[0]) / "shipyard"
    fence.write_text(
        "#!/bin/sh\n"
        f'printf \'%s\\n\' "$*" > "{seen}"\n'
        f'cat >> "{seen}"\n'
        '[ "$1 $2" = "claim check-push" ] || exit 99\n'
        f"echo 'claim-fence: stub' >&2\nexit {status}\n",
        encoding="utf-8",
    )
    fence.chmod(0o755)
    proc = subprocess.run(
        ["bash", str(HOOK), "origin", URL],
        cwd=ROOT, env=env, text=True, capture_output=True, input=REFS, timeout=120,
    )
    return proc, seen.read_text(encoding="utf-8") if seen.exists() else ""


class PrePushClaimFenceTests(unittest.TestCase):
    def test_a_refusal_stops_the_push_before_the_gates(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            proc, seen = run_with_fence(Path(td), 1)
        self.assertEqual(proc.returncode, 1, proc.stderr)
        self.assertIn("claim-fence: stub", proc.stderr)
        self.assertNotIn("gates clean", proc.stderr)
        self.assertEqual(seen, f"claim check-push origin {URL}\n{REFS}")

    def test_an_allowed_push_runs_the_gates(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            proc, _ = run_with_fence(Path(td), 0)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("gates clean", proc.stderr)

    def test_a_shipyard_without_the_command_fails_open_loudly(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            proc, _ = run_with_fence(Path(td), 2)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("claim check-push could not run; pushing without the check", proc.stderr)


if __name__ == "__main__":
    unittest.main()

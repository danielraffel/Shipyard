#!/usr/bin/env python3
"""Exercise the guarded plain gh executable, not the ghapp wrapper."""
from __future__ import annotations
import os
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ENTRY = ROOT / "scripts/gh"

with tempfile.TemporaryDirectory(prefix="shipyard-plain-gh-") as td:
    root = Path(td)
    guards = root / "guards"; guards.mkdir()
    guard = guards / "queue-removal-guard"
    guard.write_text("#!/bin/sh\ncase \"$*\" in\n  *disablePullRequestAutoMerge*|*dequeuePullRequest*) echo blocked >&2; exit 1;;\nesac\nexit 0\n")
    guard.chmod(0o755)
    native = root / "native-gh"
    native.write_text("#!/bin/sh\nprintf 'native:%s\\n' \"$*\"\n")
    native.chmod(0o755)
    env = os.environ.copy()
    env["SHIPYARD_GHAPP_GUARDS_DIR"] = str(guards)
    env["SHIPYARD_NATIVE_GH_BINARY"] = str(native)
    harmless = subprocess.run([str(ENTRY), "pr", "view", "1"], env=env, text=True, capture_output=True)
    assert harmless.returncode == 0 and "native:pr view 1" in harmless.stdout, harmless
    blocked = subprocess.run([str(ENTRY), "api", "graphql", "-f", "query=mutation { dequeuePullRequest(input:{id:\"x\"}) { clientMutationId } }"], env=env, text=True, capture_output=True)
    assert blocked.returncode != 0 and "blocked" in blocked.stderr, blocked
    assert "native:" not in blocked.stdout
print("plain gh guard: ok")

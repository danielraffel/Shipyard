#!/usr/bin/env python3
"""Exercise the installed plain gh executable with the real queue guard."""
from __future__ import annotations
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ENTRY = ROOT / "scripts/gh"
INSTALL = ROOT / "scripts/install-guarded-gh.sh"
REAL_GUARD = ROOT / "scripts/ghapp_queue_removal_guard.py"

with tempfile.TemporaryDirectory(prefix="shipyard-plain-gh-") as td:
    root = Path(td)
    home = root / "home"
    bin_dir = home / ".local/bin"
    guards_dir = home / ".config/shipyard/guards"
    native = root / "native-gh"
    native.write_text("#!/bin/sh\nprintf 'native:%s\\n' \"$*\"\n", encoding="utf-8")
    native.chmod(0o755)
    env = os.environ.copy()
    env["HOME"] = str(home)
    env.pop("SHIPYARD_GHAPP_GUARDS_DIR", None)
    env["SHIPYARD_NATIVE_GH_BINARY"] = str(native)
    install = subprocess.run([str(INSTALL), "--bin-dir", str(bin_dir), "--guards-dir", str(guards_dir)], env=env, text=True, encoding="utf-8", capture_output=True)
    assert install.returncode == 0, install.stderr
    assert (guards_dir / "queue-removal-guard").read_bytes() == REAL_GUARD.read_bytes()
    path_env = f"{bin_dir}:{env['PATH']}"
    resolved = subprocess.run(["bash", "-c", "command -v gh"], env={**env, "PATH": path_env}, text=True, encoding="utf-8", capture_output=True)
    assert Path(resolved.stdout.strip()).resolve() == (bin_dir / "gh").resolve(), resolved.stdout
    harmless = subprocess.run([str(bin_dir / "gh"), "pr", "view", "1"], env=env, text=True, encoding="utf-8", capture_output=True)
    assert harmless.returncode == 0 and "native:pr view 1" in harmless.stdout, harmless
    blocked = subprocess.run([str(bin_dir / "gh"), "api", "graphql", "-f", "query=mutation { dequeuePullRequest(input:{id:\"x\"}) { clientMutationId } }"], env=env, text=True, encoding="utf-8", capture_output=True)
    assert blocked.returncode != 0 and "unaudited merge-queue removal" in blocked.stderr, blocked
    assert "native:" not in blocked.stdout
print("plain gh install and guard: ok")

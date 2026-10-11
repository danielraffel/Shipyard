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
    hostile_python = root / "python3"
    hostile_python.write_text("#!/bin/sh\nprintf hostile-python >&2\nexit 99\n", encoding="utf-8")
    hostile_python.chmod(0o755)
    env = os.environ.copy()
    env["HOME"] = str(home)
    env.pop("SHIPYARD_GHAPP_GUARDS_DIR", None)
    env["SHIPYARD_NATIVE_GH_BINARY"] = str(native)
    install = subprocess.run([str(INSTALL), "--bin-dir", str(bin_dir)], env=env, text=True, encoding="utf-8", capture_output=True)
    assert install.returncode == 0, install.stderr
    assert not (guards_dir / "queue-removal-guard").exists()
    assert "shipyard guards install" in install.stdout
    path_env = f"{bin_dir}:{root}:{env['PATH']}"
    resolved = subprocess.run(["bash", "-c", "command -v gh"], env={**env, "PATH": path_env}, text=True, encoding="utf-8", capture_output=True)
    assert Path(resolved.stdout.strip()).resolve() == (bin_dir / "gh").resolve(), resolved.stdout
    # Use the repository's real guard for the wrapper exercise. The installer
    # above must not copy it; `shipyard guards install` owns that write in the
    # Rust entry point and is covered by the Rust guard tests.
    guards_dir.mkdir(parents=True)
    shutil.copy2(REAL_GUARD, guards_dir / "queue-removal-guard")
    (guards_dir / "queue-removal-guard").chmod(0o755)
    harmless = subprocess.run([str(bin_dir / "gh"), "pr", "view", "1"], env={**env, "PATH": path_env}, text=True, encoding="utf-8", capture_output=True)
    assert harmless.returncode == 0 and "native:pr view 1" in harmless.stdout, harmless
    blocked = subprocess.run([str(bin_dir / "gh"), "api", "graphql", "-f", "query=mutation { dequeuePullRequest(input:{id:\"x\"}) { clientMutationId } }"], env={**env, "PATH": path_env}, text=True, encoding="utf-8", capture_output=True)
    assert blocked.returncode != 0 and "unaudited merge-queue removal" in blocked.stderr, blocked
    assert "native:" not in blocked.stdout
    assert "hostile-python" not in blocked.stderr
print("plain gh install and guard: ok")

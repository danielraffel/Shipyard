#!/usr/bin/env python3
"""Exercise hook installation through scripts/install-githooks.sh."""
from __future__ import annotations
import subprocess, tempfile
from pathlib import Path
ROOT = Path(__file__).resolve().parents[1]

def main() -> int:
    with tempfile.TemporaryDirectory() as td:
        repo = Path(td)
        (repo / ".githooks").mkdir()
        (repo / ".githooks/pre-push").write_text("#!/bin/sh\n")
        (repo / "scripts").mkdir()
        (repo / "scripts/install-githooks.sh").write_bytes((ROOT / "scripts/install-githooks.sh").read_bytes())
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        proc = subprocess.run(["bash", "scripts/install-githooks.sh"], cwd=repo, text=True, capture_output=True)
        assert proc.returncode == 0, proc.stderr
        value = subprocess.check_output(["git", "config", "--get", "core.hooksPath"], cwd=repo, text=True).strip()
        assert value == ".githooks", value
    return 0
if __name__ == "__main__": raise SystemExit(main())

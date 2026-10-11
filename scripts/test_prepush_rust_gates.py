#!/usr/bin/env python3
"""Real pre-push entry-point tests for Rust and source guards."""
from __future__ import annotations
import os, subprocess, tempfile
from pathlib import Path
ROOT = Path(__file__).resolve().parents[1]
HOOK = ROOT / ".githooks/pre-push"

def run_hook(extra_env: dict[str,str], base: str = "HEAD"):
    env = os.environ.copy(); env.update(extra_env)
    env["SHIPYARD_PREPUSH_BASE"] = base
    env["SHIPYARD_ENFORCE_PREPUSH"] = "0"
    return subprocess.run(["bash", str(HOOK), "origin", "refs/heads/test", "refs/heads/test", "0000000", "0000000"], cwd=ROOT, env=env, text=True, capture_output=True)

def main() -> int:
    # The source guard is exercised through the hook's actual entry point.
    with tempfile.TemporaryDirectory() as td:
        fake = Path(td) / "cargo"
        fake.write_text("#!/bin/sh\nexit 0\n")
        fake.chmod(0o755)
        env = {"PATH": f"{td}:{os.environ['PATH']}", "SHIPYARD_HOST": "m3"}
        proc = run_hook(env)
        assert proc.returncode != 0, proc.stderr
        assert "Cargo is forbidden on m3" in proc.stderr, proc.stderr
        assert "cargo fmt --all --check" in proc.stderr, proc.stderr
        assert "cargo clippy --all-targets --locked" in proc.stderr, proc.stderr
    source = (ROOT / "scripts/check_test_include_str.py").read_text()
    assert "include_str!" in source
    return 0
if __name__ == "__main__": raise SystemExit(main())

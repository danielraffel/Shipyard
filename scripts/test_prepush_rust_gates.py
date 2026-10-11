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
        target = ROOT / "src/app.rs"
        original = target.read_bytes()
        try:
            target.write_bytes(original + b"\n")
            proc = run_hook(env)
            assert proc.returncode != 0, proc.stderr
            assert "Cargo is forbidden on m3" in proc.stderr, proc.stderr
            assert "cargo fmt --all --check" in proc.stderr, proc.stderr
            assert "advisory clippy" in proc.stderr and "cargo clippy --all-targets --locked" in proc.stderr, proc.stderr
        finally:
            target.write_bytes(original)
    checker = ROOT / "scripts/check_test_include_str.py"
    with tempfile.TemporaryDirectory() as td:
        repo = Path(td); (repo / "src").mkdir()
        cases = [
            "#[cfg(test)]\nconst S: &str = include_str!(\"x.rs\");\n",
            "#[test]\nfn bad() { let _ = Command::new(\"/bin/sh\"); }\n",
            "#[cfg(test)]\nif injected() { return; }\n",
        ]
        for index, contents in enumerate(cases):
            path = repo / "src" / f"bad{index}.rs"; path.write_text(contents)
            proc = subprocess.run(["python3", str(checker), f"src/bad{index}.rs"], cwd=repo, text=True, capture_output=True)
            assert proc.returncode == 1, (index, proc.stdout, proc.stderr)
        good = repo / "src" / "good.rs"; good.write_text("#[cfg(test)]\nmod tests { #[test] fn ok() {} }\n")
        proc = subprocess.run(["python3", str(checker), "src/good.rs"], cwd=repo, text=True, capture_output=True)
        assert proc.returncode == 0, proc.stderr
    return 0
if __name__ == "__main__": raise SystemExit(main())

#!/usr/bin/env python3
"""Exercise the pre-push hook, including real blocking failures."""
from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
HOOK = ROOT / ".githooks/pre-push"


def run_hook(extra_env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env.update(extra_env)
    env["SHIPYARD_PREPUSH_BASE"] = "HEAD"
    env["SHIPYARD_ENFORCE_PREPUSH"] = "0"
    return subprocess.run(
        ["bash", str(HOOK), "origin", "refs/heads/test", "refs/heads/test", "0000000", "0000000"],
        cwd=ROOT, env=env, text=True, capture_output=True,
    )


class PrePushRustGateTests(unittest.TestCase):
    def fake_cargo(self, directory: Path, fmt_status: int = 0) -> None:
        cargo = directory / "cargo"
        cargo.write_text(
            "#!/bin/sh\n"
            f"case \"$1\" in fmt) exit {fmt_status};; esac\n"
            "exit 0\n", encoding="utf-8",
        )
        cargo.chmod(0o755)

    def test_real_fmt_failure_refuses(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            directory = Path(td)
            self.fake_cargo(directory, fmt_status=1)
            proc = run_hook({"PATH": f"{directory}:{os.environ['PATH']}"})
            self.assertNotEqual(proc.returncode, 0, proc.stderr)
            self.assertIn("blocking Rust/source gates failed", proc.stderr)

    def test_real_changed_test_lint_refuses(self) -> None:
        target = ROOT / "src/app.rs"
        original = target.read_bytes()
        try:
            target.write_bytes(
                original
                + b'\n#[cfg(test)]\nmod planted_prepush_test {\n'
                + b'    #[test]\n    fn shell_spawn_is_rejected() {\n'
                + b'        let _ = std::process::Command::new("/bin/sh");\n'
                + b'    }\n}\n'
            )
            with tempfile.TemporaryDirectory() as td:
                directory = Path(td)
                self.fake_cargo(directory)
                proc = run_hook({"PATH": f"{directory}:{os.environ['PATH']}"})
            self.assertNotEqual(proc.returncode, 0, proc.stderr)
            self.assertIn("/bin/sh is forbidden inside test code", proc.stderr)
        finally:
            target.write_bytes(original)

    def test_source_lints_cover_include_cfg_return_and_shell_scope(self) -> None:
        checker = ROOT / "scripts/check_test_include_str.py"
        with tempfile.TemporaryDirectory() as td:
            repo = Path(td)
            (repo / "src").mkdir()
            bad = repo / "src/bad.rs"
            bad.write_text(
                "#[cfg(test)]\nmod tests {\n"
                '    const SOURCE: &str = include_str!("fixture.rs");\n'
                '    #[test] fn shell() { let _ = Command::new("/bin/sh"); }\n'
                "}\n"
                "#[cfg(test)]\nif injected() { return; }\n",
                encoding="utf-8",
            )
            proc = subprocess.run(
                ["python3", str(checker), "src/bad.rs"],
                cwd=repo, text=True, capture_output=True,
            )
            self.assertEqual(proc.returncode, 1, proc.stderr)
            self.assertIn("must not include Rust source", proc.stderr)
            self.assertIn("/bin/sh is forbidden inside test code", proc.stderr)
            self.assertIn("cfg(test) if/return", proc.stderr)
            good = repo / "src/good.rs"
            good.write_text('fn production() { let _ = Command::new("/bin/sh"); }\n', encoding="utf-8")
            proc = subprocess.run(
                ["python3", str(checker), "src/good.rs"],
                cwd=repo, text=True, capture_output=True,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)

    def test_missing_pinned_toolchain_prints_install_command(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            cargo_bin = root / "home/.cargo/bin"
            cargo_bin.mkdir(parents=True)
            rustup = cargo_bin / "rustup"
            rustup.write_text("#!/bin/sh\n[ \"$1\" = toolchain ] && exit 0\n", encoding="utf-8")
            rustup.chmod(0o755)
            (cargo_bin / "rustc").symlink_to(rustup)
            self.fake_cargo(root)
            env = {"HOME": str(root / "home"), "PATH": f"{cargo_bin}:{root}:{os.environ['PATH']}"}
            proc = run_hook(env)
            self.assertNotEqual(proc.returncode, 0, proc.stderr)
            self.assertIn("rustup toolchain install 1.97.1", proc.stderr)


if __name__ == "__main__":
    unittest.main()

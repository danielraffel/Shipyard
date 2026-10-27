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


def run_hook(extra_env: dict[str, str], timeout: float | None = None) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env.pop("RUSTUP_AUTO_INSTALL", None)
    env.update(extra_env)
    env["SHIPYARD_PREPUSH_BASE"] = "HEAD"
    env["SHIPYARD_ENFORCE_PREPUSH"] = "0"
    return subprocess.run(
        ["bash", str(HOOK), "origin", "refs/heads/test", "refs/heads/test", "0000000", "0000000"],
        cwd=ROOT, env=env, text=True, capture_output=True, timeout=timeout,
    )


# `rustup toolchain list` as rustup 1.29 prints it: channel plus host triple.
REAL_LIST = "stable-aarch64-apple-darwin (active, default)\n1.88.0-aarch64-apple-darwin\n"
PINNED_LIST = REAL_LIST + "1.97.1-aarch64-apple-darwin\n"


def provision(
    root: Path,
    *,
    toolchains: str = PINNED_LIST,
    fmt_status: int = 0,
    clippy_status: int = 0,
    rustc_is_proxy: bool = True,
    rustc_proxy_kind: str = "symlink",
) -> dict[str, str]:
    """A HOME whose rustup proxy lists `toolchains`, plus a fake cargo."""
    cargo_bin = root / "home/.cargo/bin"
    cargo_bin.mkdir(parents=True)
    listing = root / "toolchains.txt"
    listing.write_text(toolchains, encoding="utf-8")
    rustup = cargo_bin / "rustup"
    rustup.write_text(
        "#!/bin/sh\n"
        f'[ "$1 $2" = "toolchain list" ] && cat "{listing}"\n'
        "exit 0\n",
        encoding="utf-8",
    )
    rustup.chmod(0o755)
    rustc = cargo_bin / "rustc"
    if rustc_is_proxy and rustc_proxy_kind == "symlink":
        rustc.symlink_to("rustup")
    elif rustc_is_proxy and rustc_proxy_kind == "hardlink":
        os.link(rustup, rustc)
    elif rustc_is_proxy:
        rustc.write_bytes(rustup.read_bytes())
        rustc.chmod(0o755)
    else:
        rustc.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        rustc.chmod(0o755)
    cargo = cargo_bin / "cargo"
    # Like the real rustup proxy, cargo would download a missing pinned
    # toolchain unless RUSTUP_AUTO_INSTALL=0; the sleep stands in for that
    # download, and every call is logged with the setting it saw.
    calls = root / "cargo-calls.txt"
    pinned = "1.97.1-" in toolchains
    cargo.write_text(
        "#!/bin/sh\n"
        f'printf \'%s auto_install=%s\\n\' "$1" "${{RUSTUP_AUTO_INSTALL:-unset}}" >> "{calls}"\n'
        + ("" if pinned else '[ "${RUSTUP_AUTO_INSTALL:-1}" = 0 ] || sleep 60\n')
        + f'case "$1" in fmt) exit {fmt_status};; clippy) exit {clippy_status};; esac\n'
        "exit 0\n",
        encoding="utf-8",
    )
    cargo.chmod(0o755)
    return {"HOME": str(root / "home"), "PATH": f"{cargo_bin}:{os.environ['PATH']}"}


class PlantedSourceChange:
    """Append a harmless line to a tracked `src/` file so clippy runs."""

    def __init__(self, extra: bytes = b"\n") -> None:
        self.target = ROOT / "src/app.rs"
        self.extra = extra

    def __enter__(self) -> None:
        self.original = self.target.read_bytes()
        self.target.write_bytes(self.original + self.extra)

    def __exit__(self, *_: object) -> None:
        self.target.write_bytes(self.original)


class PrePushRustGateTests(unittest.TestCase):
    def test_a_provisioned_host_with_clean_gates_passes(self) -> None:
        # Positive control: the real `rustup toolchain list` shape names the
        # pinned channel with its host triple, and that counts as installed.
        with tempfile.TemporaryDirectory() as td, PlantedSourceChange():
            proc = run_hook(provision(Path(td)))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("gates clean", proc.stderr)

    def test_missing_pinned_toolchain_prints_install_command(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            proc = run_hook(provision(Path(td), toolchains=REAL_LIST))
        self.assertNotEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("pinned Rust 1.97.1 is not installed", proc.stderr)
        self.assertIn("rustup toolchain install 1.97.1", proc.stderr)
        self.assertNotIn("must be the rustup proxy", proc.stderr)

    def test_a_missing_pin_refuses_at_once_without_running_cargo(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            proc = run_hook(provision(root, toolchains=REAL_LIST), timeout=30)
            calls = root / "cargo-calls.txt"
            self.assertNotEqual(proc.returncode, 0, proc.stderr)
            self.assertIn("skipping cargo gates", proc.stderr)
            self.assertFalse(calls.exists(), calls.read_text() if calls.exists() else "")

    def test_cargo_never_runs_with_rustup_auto_install(self) -> None:
        with tempfile.TemporaryDirectory() as td, PlantedSourceChange():
            root = Path(td)
            proc = run_hook(provision(root), timeout=30)
            calls = (root / "cargo-calls.txt").read_text().splitlines()
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(calls, ["fmt auto_install=0", "clippy auto_install=0"])

    def test_a_hardlinked_or_copied_rustup_proxy_is_accepted(self) -> None:
        for kind in ("hardlink", "copy"):
            with tempfile.TemporaryDirectory() as td:
                proc = run_hook(provision(Path(td), rustc_proxy_kind=kind))
            self.assertEqual(proc.returncode, 0, f"{kind}: {proc.stderr}")
            self.assertNotIn("must be the rustup proxy", proc.stderr)

    def test_a_faster_host_can_run_the_pinned_gate_without_m1(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            env = provision(Path(td))
            env["SHIPYARD_HOST"] = "m3"
            proc = run_hook(env)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertNotIn("Rust gates require m1", proc.stderr)
        self.assertIn("gates clean", proc.stderr)

    def test_a_longer_channel_with_the_pin_as_prefix_is_not_the_pin(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            proc = run_hook(provision(Path(td), toolchains=REAL_LIST + "1.97.10-aarch64-apple-darwin\n"))
        self.assertIn("pinned Rust 1.97.1 is not installed", proc.stderr)

    def test_real_fmt_failure_refuses(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            proc = run_hook(provision(Path(td), fmt_status=1))
        self.assertNotEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("cargo fmt --check failed", proc.stderr)
        self.assertNotIn("is not installed", proc.stderr)

    def test_real_clippy_failure_refuses(self) -> None:
        with tempfile.TemporaryDirectory() as td, PlantedSourceChange():
            proc = run_hook(provision(Path(td), clippy_status=1))
        self.assertNotEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("cargo clippy -D warnings failed", proc.stderr)
        self.assertNotIn("cargo fmt --check failed", proc.stderr)

    def test_a_rustc_that_is_not_the_rustup_proxy_refuses(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            proc = run_hook(provision(Path(td), rustc_is_proxy=False))
        self.assertNotEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("must be the rustup proxy", proc.stderr)
        self.assertNotIn("is not installed", proc.stderr)

    def test_real_changed_test_lint_refuses(self) -> None:
        planted = (
            b'\n#[cfg(test)]\nmod planted_prepush_test {\n'
            b'    #[test]\n    fn shell_spawn_is_rejected() {\n'
            b'        let _ = std::process::Command::new("/bin/sh");\n'
            b'    }\n}\n'
        )
        with tempfile.TemporaryDirectory() as td, PlantedSourceChange(planted):
            proc = run_hook(provision(Path(td)))
        self.assertNotEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("/bin/sh is forbidden inside test code", proc.stderr)

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

    def test_a_deletion_only_diff_lints_no_untouched_line(self) -> None:
        checker = ROOT / "scripts/check_test_include_str.py"
        old = (
            "#[cfg(test)]\nmod tests {\n\n"
            '    #[test] fn shell() { let _ = Command::new("/bin/sh"); }\n'
            "}\n"
        )

        def check(repo: Path, *args: str) -> subprocess.CompletedProcess[str]:
            return subprocess.run(
                ["python3", str(checker), *args, "src/old.rs"],
                cwd=repo, text=True, capture_output=True,
            )

        with tempfile.TemporaryDirectory() as td:
            repo = Path(td)
            git = lambda *a: subprocess.run(["git", *a], cwd=repo, check=True, capture_output=True)
            git("init", "-q")
            git("-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "-q", "--allow-empty", "-m", "root")
            (repo / "src").mkdir()
            source = repo / "src/old.rs"
            source.write_text(old, encoding="utf-8")
            git("add", ".")
            git("-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "-q", "-m", "old")
            # Remove only the blank line: the pre-existing violation is untouched.
            source.write_text(old.replace("{\n\n", "{\n", 1), encoding="utf-8")
            proc = check(repo, "--base", "HEAD")
            self.assertEqual(proc.returncode, 0, proc.stderr)
            # Control: adding a violating line in the same file is still caught.
            source.write_text(
                old.removesuffix("}\n")
                + '    #[test] fn again() { let _ = Command::new("/bin/sh"); }\n}\n',
                encoding="utf-8",
            )
            proc = check(repo, "--base", "HEAD")
            self.assertEqual(proc.returncode, 1, proc.stderr)
            self.assertIn("src/old.rs:5:", proc.stderr)
            self.assertNotIn("src/old.rs:4:", proc.stderr)
            # An unreadable base cannot prove any line unchanged.
            source.write_text(old.replace("{\n\n", "{\n", 1), encoding="utf-8")
            proc = check(repo, "--base", "no-such-ref")
            self.assertEqual(proc.returncode, 1, proc.stderr)

if __name__ == "__main__":
    unittest.main()

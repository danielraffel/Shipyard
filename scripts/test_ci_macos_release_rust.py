#!/usr/bin/env python3
"""Tests for the fixed-path release toolchain installer, using stub rustup binaries."""

from __future__ import annotations

import contextlib
import fcntl
import hashlib
import io
import os
import stat
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import ci_macos_release_rust as installer  # noqa: E402

TOOLCHAIN = "1.99.1"
HOST = "aarch64-apple-darwin"
RUSTUP_VERSION = "1.29.3"

# One stub binary plays both roles, as the real one does: run as
# ``rustup-init`` it installs a copy of itself as ``$CARGO_HOME/bin/rustup``.
STUB_RUSTUP_INIT = textwrap.dedent(
    """\
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "$(basename "$0")" = rustup-init ]; then
      echo "rustup-init $*" >> "$STUB_LOG"
      mkdir -p "$CARGO_HOME/bin"
      cp "$0" "$CARGO_HOME/bin/rustup"
      chmod +x "$CARGO_HOME/bin/rustup"
      exit 0
    fi
    case "$1" in
      --version) echo "rustup ${STUB_RUSTUP_VERSION} (stub 2026-10-01)"; exit 0 ;;
    esac
    echo "rustup $*" >> "$STUB_LOG"
    echo "env DIST_SERVER=${RUSTUP_DIST_SERVER:-} UPDATE_ROOT=${RUSTUP_UPDATE_ROOT:-} DIST_ROOT=${RUSTUP_DIST_ROOT:-unset}" >> "$STUB_LOG"
    case "$1" in
      set) exit 0 ;;
      toolchain)
        if [ -e "$STUB_FAIL_ONCE" ]; then rm -f "$STUB_FAIL_ONCE"; exit 1; fi
        bin="$RUSTUP_HOME/toolchains/$3/bin"
        mkdir -p "$bin"
        printf '#!/bin/sh\\necho "rustc %s (stub 2026-10-13)"\\n' "$STUB_VERSION" > "$bin/rustc"
        printf '#!/bin/sh\\necho "cargo %s (stub 2026-10-13)"\\n' "$STUB_VERSION" > "$bin/cargo"
        chmod +x "$bin/rustc" "$bin/cargo"
        ;;
      *) exit 2 ;;
    esac
    """
)


class InstallerTests(unittest.TestCase):
    def setUp(self) -> None:
        self._temp = tempfile.TemporaryDirectory(prefix="shipyard-release-rust-test-")
        self.base = Path(self._temp.name)
        self.addCleanup(self._cleanup)
        self.root = self.base / "home" / ".shipyard-release-rust"
        self.log = self.base / "stub.log"
        self.fail_once = self.base / "fail-once"
        self.init = self.base / "rustup-init"
        self.init.write_text(STUB_RUSTUP_INIT, encoding="utf-8")
        self.init.chmod(0o755)
        self.init_sha = hashlib.sha256(self.init.read_bytes()).hexdigest()
        self.env_file = self.base / "github_env"
        self.path_file = self.base / "github_path"
        self.run_number = 0
        self._saved_env = {k: os.environ.get(k) for k in ("STUB_LOG", "STUB_FAIL_ONCE", "STUB_VERSION",
                                                       "STUB_RUSTUP_VERSION", "RUSTUP_DIST_SERVER",
                                                       "RUSTUP_DIST_ROOT", "RUSTUP_UPDATE_ROOT")}
        os.environ["STUB_LOG"] = str(self.log)
        os.environ["STUB_FAIL_ONCE"] = str(self.fail_once)
        os.environ["STUB_VERSION"] = TOOLCHAIN
        os.environ["STUB_RUSTUP_VERSION"] = RUSTUP_VERSION

    def _cleanup(self) -> None:
        for key, value in self._saved_env.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value
        installer.set_writable(self.base, True)
        self._temp.cleanup()

    def install(self, *, sha: str | None = None, run_cargo_home: Path | None = None,
                lock_wait: float = 5, rustup_version: str = RUSTUP_VERSION,
                init: Path | None = None) -> int:
        self.run_number += 1
        cargo_home = run_cargo_home or self.base / "runner-temp" / f"shipyard-cargo-{self.run_number}"
        return installer.main([
            "--root", str(self.root),
            "--toolchain", TOOLCHAIN,
            "--host", HOST,
            "--rustup-version", rustup_version,
            "--rustup-init-sha256", sha or self.init_sha,
            "--run-cargo-home", str(cargo_home),
            "--github-env", str(self.env_file),
            "--github-path", str(self.path_file),
            "--rustup-init-url", (init or self.init).as_uri(),
            "--backoff", "0,0",
            "--lock-wait", str(lock_wait),
        ])

    def calls(self, prefix: str) -> int:
        if not self.log.exists():
            return 0
        return sum(1 for line in self.log.read_text().splitlines() if line.startswith(prefix))

    @property
    def toolchain_dir(self) -> Path:
        return self.root / "rustup" / "toolchains" / f"{TOOLCHAIN}-{HOST}"

    def test_fresh_install_exports_fixed_toolchain_and_per_run_crates(self) -> None:
        self.assertEqual(self.install(), 0)
        bin_dir = self.toolchain_dir / "bin"
        self.assertEqual(self.path_file.read_text().splitlines(), [str(bin_dir)])
        env = dict(line.split("=", 1) for line in self.env_file.read_text().splitlines())
        self.assertEqual(env["RUSTUP_HOME"], str(self.root / "rustup"))
        self.assertEqual(env["RUSTUP_TOOLCHAIN"], f"{TOOLCHAIN}-{HOST}")
        self.assertEqual(env["RUSTC"], str(bin_dir / "rustc"))
        self.assertEqual(env["CARGO_HOME"], str(self.base / "runner-temp" / "shipyard-cargo-1"))
        self.assertTrue(Path(env["CARGO_HOME"]).is_dir())
        self.assertNotIn(self.root, Path(env["CARGO_HOME"]).parents)
        self.assertTrue((self.root / "cargo" / "bin" / "rustup").is_file())
        self.assertEqual(stat.S_IMODE(self.root.stat().st_mode), 0o700)
        # The installed toolchain is read-only.
        self.assertFalse((bin_dir / "rustc").stat().st_mode & stat.S_IWUSR)
        self.assertIn(f"rustup toolchain install {TOOLCHAIN}-{HOST} --profile minimal --no-self-update",
                      self.log.read_text())

    def test_second_run_reuses_rustup_and_toolchain_without_network(self) -> None:
        self.install()
        self.install()
        self.assertEqual(self.calls("rustup-init"), 1)
        self.assertEqual(self.calls("rustup toolchain install"), 1)
        # Each run still gets its own fresh crate home.
        homes = [line for line in self.env_file.read_text().splitlines() if line.startswith("CARGO_HOME=")]
        self.assertEqual(len(set(homes)), 2)

    def test_rustup_init_with_wrong_sha_is_never_run(self) -> None:
        with self.assertRaises(SystemExit) as raised:
            self.install(sha="0" * 64)
        self.assertIn("SHA-256", str(raised.exception))
        self.assertEqual(self.calls("rustup-init"), 0)
        self.assertFalse((self.root / "cargo" / "bin" / "rustup").exists())
        self.assertFalse(self.env_file.exists())

    def test_modified_complete_toolchain_fails_closed_without_reinstalling(self) -> None:
        self.install()
        rustc = self.toolchain_dir / "bin" / "rustc"
        rustc.chmod(0o755)
        rustc.write_text("#!/bin/sh\necho tampered\n")
        out = io.StringIO()
        with contextlib.redirect_stdout(out), self.assertRaises(SystemExit) as raised:
            self.install()
        self.assertIn("digest", str(raised.exception))
        self.assertIn("::error::", out.getvalue())
        self.assertEqual(self.calls("rustup toolchain install"), 1)
        # Left in place for a human to inspect, and nothing is exported to the job.
        self.assertIn("tampered", rustc.read_text())
        self.assertEqual(self.path_file.read_text().splitlines().count(str(rustc.parent)), 1)

    def test_incomplete_toolchain_is_reinstalled_with_a_warning(self) -> None:
        # A toolchain dir with no completion marker: an interrupted install.
        (self.toolchain_dir / "bin").mkdir(parents=True)
        (self.toolchain_dir / "bin" / "rustc").write_text("partial")
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(self.install(), 0)
        self.assertIn("::warning::", out.getvalue())
        self.assertEqual(self.calls("rustup toolchain install"), 1)
        self.assertNotEqual((self.toolchain_dir / "bin" / "rustc").read_text(), "partial")

    def test_modified_rustup_is_reinstalled_from_pinned_init(self) -> None:
        self.install()
        rustup = self.root / "cargo" / "bin" / "rustup"
        rustup.write_text(rustup.read_text() + "# changed\n")
        self.install()
        self.assertEqual(self.calls("rustup-init"), 2)
        self.assertNotIn("# changed", rustup.read_text())

    def test_reused_rustup_is_checked_against_the_pin_not_a_marker(self) -> None:
        self.install()
        self.assertFalse((self.root / "rustup.sha256").exists())
        # Bump the pin in the workflow: a new rustup-init with a new SHA.
        bumped = self.base / "bumped" / "rustup-init"
        bumped.parent.mkdir()
        bumped.write_text(STUB_RUSTUP_INIT + "# 1.29.4\n")
        bumped.chmod(0o755)
        os.environ["STUB_RUSTUP_VERSION"] = "1.29.4"
        self.install(sha=hashlib.sha256(bumped.read_bytes()).hexdigest(),
                     rustup_version="1.29.4", init=bumped)
        self.assertEqual(self.calls("rustup-init"), 2)
        self.assertEqual((self.root / "cargo" / "bin" / "rustup").read_bytes(), bumped.read_bytes())

    def test_reused_rustup_must_report_the_pinned_version(self) -> None:
        self.install()
        os.environ["STUB_RUSTUP_VERSION"] = "1.28.0"
        with self.assertRaises(SystemExit) as raised:
            self.install()
        self.assertIn("expected rustup 1.29.3", str(raised.exception))

    def test_inherited_rustup_servers_are_replaced_with_official_ones(self) -> None:
        os.environ["RUSTUP_DIST_SERVER"] = "https://evil.invalid"
        os.environ["RUSTUP_UPDATE_ROOT"] = "https://evil.invalid/rustup"
        os.environ["RUSTUP_DIST_ROOT"] = "https://evil.invalid/dist"
        self.install()
        lines = [line for line in self.log.read_text().splitlines() if line.startswith("env ")]
        self.assertTrue(lines)
        for line in lines:
            self.assertEqual(
                line,
                "env DIST_SERVER=https://static.rust-lang.org "
                "UPDATE_ROOT=https://static.rust-lang.org/rustup DIST_ROOT=unset",
            )

    def test_wrong_toolchain_version_fails_closed(self) -> None:
        os.environ["STUB_VERSION"] = "1.98.0"
        with self.assertRaises(SystemExit) as raised:
            self.install()
        self.assertIn("expected rustc 1.99.1", str(raised.exception))
        self.assertFalse((self.root / f"toolchain-{TOOLCHAIN}-{HOST}.sha256").exists())
        self.assertFalse(self.path_file.exists())

    def test_transient_install_failure_is_retried(self) -> None:
        self.fail_once.write_text("1")
        self.assertEqual(self.install(), 0)
        self.assertEqual(self.calls("rustup toolchain install"), 2)

    def test_install_waits_for_the_host_lock(self) -> None:
        self.root.mkdir(parents=True, mode=0o700)
        with (self.root / ".install.lock").open("a+") as held:
            fcntl.flock(held.fileno(), fcntl.LOCK_EX)
            with self.assertRaises(SystemExit) as raised:
                self.install(lock_wait=0.5)
        self.assertIn(".install.lock", str(raised.exception))
        self.assertEqual(self.calls("rustup"), 0)
        # Released lock: the next job proceeds.
        self.assertEqual(self.install(), 0)

    def test_per_run_cargo_home_must_be_outside_the_shared_root(self) -> None:
        with self.assertRaises(SystemExit) as raised:
            self.install(run_cargo_home=self.root / "cargo")
        self.assertIn("outside --root", str(raised.exception))

    def test_symlinked_root_is_refused(self) -> None:
        target = self.base / "elsewhere"
        target.mkdir()
        self.root.parent.mkdir(parents=True)
        self.root.symlink_to(target)
        with self.assertRaises(SystemExit) as raised:
            self.install()
        self.assertIn("symlink", str(raised.exception))


if __name__ == "__main__":
    unittest.main()

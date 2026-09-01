#!/usr/bin/env python3
"""Regression tests for Shipyard's dogfood validation toolchain selector."""

from __future__ import annotations

import os
import pathlib
import subprocess
import tempfile
import textwrap
import tomllib
import unittest


REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent


def _fake_toolchain(root: pathlib.Path, name: str, rustc_version: str) -> pathlib.Path:
    bin_dir = root / ".rustup" / "toolchains" / name / "bin"
    bin_dir.mkdir(parents=True)
    (bin_dir / "rustc").write_text(
        f"#!/bin/sh\nprintf '%s\\n' 'rustc {rustc_version} (fake)'\n"
    )
    (bin_dir / "cargo").write_text(
        textwrap.dedent(
            f"""\
            #!/bin/sh
            printf '%s\\n' '{name}:$*' >> "$SHIPYARD_TOOLCHAIN_TEST_LOG"
            exit 0
            """
        )
    )
    (bin_dir / "rustc").chmod(0o755)
    (bin_dir / "cargo").chmod(0o755)
    return bin_dir


class SelfValidationToolchain(unittest.TestCase):
    def test_skips_working_cargo_whose_rustc_is_below_msrv(self):
        with tempfile.TemporaryDirectory() as raw_root:
            root = pathlib.Path(raw_root)
            old_bin = _fake_toolchain(root, "1.88.0-test", "1.88.0")
            _fake_toolchain(root, "1.92.0-test", "1.92.0")
            (root / "Cargo.toml").write_text(
                '[package]\nname = "fixture"\nversion = "0.1.0"\nrust-version = "1.92"\n'
            )
            log = root / "cargo.log"
            command = tomllib.loads(
                (REPO_ROOT / ".shipyard" / "config.toml").read_text()
            )["validation"]["default"]["command"]
            env = os.environ.copy()
            env.update(
                {
                    "HOME": str(root),
                    "PATH": f"{old_bin}:/usr/bin:/bin:/usr/sbin:/sbin",
                    "SHIPYARD_TOOLCHAIN_TEST_LOG": str(log),
                }
            )
            completed = subprocess.run(
                ["/bin/sh", "-c", command],
                cwd=root,
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            invocations = log.read_text().splitlines()
            self.assertTrue(invocations)
            self.assertTrue(
                all(line.startswith("1.92.0-test:") for line in invocations),
                invocations,
            )


if __name__ == "__main__":
    unittest.main()

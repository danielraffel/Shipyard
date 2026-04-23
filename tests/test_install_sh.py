"""Tests for the environment-variable contract of install.sh.

install.sh's default install location and version resolution are what
downstream consumers (Claude Code plugin's auto-installer, Codex one-
liner, project pinners like pulp) depend on. Regressions here either
fragment the install footprint (multiple shipyard binaries in
different places) or break version-pinned installers.

We drive install.sh with ``SHIPYARD_DRY_RUN=1`` which skips the
network + filesystem work and prints the resolved config as
KEY=value pairs. Platform detection (OS=macos/linux/windows) is
whatever host runs the test; we only assert invariants that hold on
every platform.
"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import pytest

# install.sh is POSIX shell and the tests drive it via `bash`. On
# Windows, Git-for-Windows bash exits non-zero on the very first
# `uname -m` resolution, and Windows doesn't populate `$HOME` so
# assertions that derive the expected path from `os.environ["HOME"]`
# throw KeyError. The installer itself isn't shipped for Windows
# users — they use the winget/msi path (when that exists) or the
# plugin's bundled binary. Linux + macOS coverage here is enough.
pytestmark = pytest.mark.skipif(
    sys.platform == "win32",
    reason="install.sh is a POSIX shell script; Linux+macOS runners provide full coverage",
)

REPO_ROOT = Path(__file__).resolve().parents[1]
INSTALL_SH = REPO_ROOT / "install.sh"


def _run_dry(env: dict[str, str] | None = None) -> dict[str, str]:
    """Run install.sh in dry-run mode; parse KEY=value output."""
    merged_env = {**os.environ, "SHIPYARD_DRY_RUN": "1"}
    if env:
        merged_env.update(env)
    result = subprocess.run(
        ["bash", str(INSTALL_SH)],
        env=merged_env,
        capture_output=True,
        text=True,
        check=True,
    )
    out: dict[str, str] = {}
    for line in result.stdout.splitlines():
        if "=" in line:
            key, _, value = line.partition("=")
            out[key.strip()] = value.strip()
    return out


def test_default_install_dir_is_local_bin() -> None:
    # The canonical install location is `~/.local/bin`. Downstream
    # consumers (plugin's check-cli.sh, Codex one-liner, any wrapper)
    # rely on this. Changing the default is a compatibility break.
    home = os.environ["HOME"]
    config = _run_dry()
    assert config["INSTALL_DIR"] == f"{home}/.local/bin"


def test_shipyard_install_dir_env_overrides(tmp_path: Path) -> None:
    config = _run_dry({"SHIPYARD_INSTALL_DIR": str(tmp_path / "bin")})
    assert config["INSTALL_DIR"] == str(tmp_path / "bin")


def test_default_version_resolves_to_latest() -> None:
    config = _run_dry()
    assert config["VERSION_LABEL"] == "latest"
    assert config["API_PATH"] == "releases/latest"


def test_explicit_latest_matches_default() -> None:
    config = _run_dry({"SHIPYARD_VERSION": "latest"})
    assert config["API_PATH"] == "releases/latest"


@pytest.mark.parametrize(
    "raw,expected_label,expected_api",
    [
        ("v0.22.1", "v0.22.1", "releases/tags/v0.22.1"),
        ("0.22.1", "v0.22.1", "releases/tags/v0.22.1"),  # shorthand normalization
        ("v1.0.0-rc.1", "v1.0.0-rc.1", "releases/tags/v1.0.0-rc.1"),
    ],
)
def test_shipyard_version_pins_specific_tag(
    raw: str, expected_label: str, expected_api: str
) -> None:
    config = _run_dry({"SHIPYARD_VERSION": raw})
    assert config["VERSION_LABEL"] == expected_label
    assert config["API_PATH"] == expected_api


def test_empty_shipyard_version_falls_back_to_latest() -> None:
    config = _run_dry({"SHIPYARD_VERSION": ""})
    assert config["API_PATH"] == "releases/latest"


def test_artifact_matches_platform() -> None:
    # ARTIFACT should always start with "shipyard-" and combine the
    # detected OS + ARCH. Exact values depend on the test host.
    config = _run_dry()
    assert config["ARTIFACT"].startswith("shipyard-")
    assert config["OS"] in ("macos", "linux", "windows")
    assert config["ARCH"] in ("arm64", "x64")
    assert config["ARTIFACT"] == f"shipyard-{config['OS']}-{config['ARCH']}"


def test_install_dir_override_does_not_affect_version_resolution() -> None:
    # Sanity: env vars are independent.
    config = _run_dry(
        {
            "SHIPYARD_INSTALL_DIR": "/tmp/foo",
            "SHIPYARD_VERSION": "v0.22.1",
        }
    )
    assert config["INSTALL_DIR"] == "/tmp/foo"
    assert config["API_PATH"] == "releases/tags/v0.22.1"

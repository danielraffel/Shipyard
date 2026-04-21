"""Runner / lifecycle tests — daemon PID semantics + stop_running."""

from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest

from shipyard.daemon.runner import _spawn_argv, daemon_is_running, stop_running

# stop_running uses AF_UNIX sockets on the happy path. PID semantics
# work on Windows but the full suite wasn't designed for it.
pytestmark = pytest.mark.skipif(
    sys.platform == "win32",
    reason="daemon lifecycle uses AF_UNIX sockets (macOS/Linux only)",
)


def test_no_pid_file_means_not_running(tmp_path: Path) -> None:
    assert not daemon_is_running(tmp_path)


def test_stale_pid_file_treated_as_not_running(tmp_path: Path) -> None:
    pid_dir = tmp_path / "daemon"
    pid_dir.mkdir(parents=True)
    # A PID that's vanishingly unlikely to belong to a real process.
    (pid_dir / "daemon.pid").write_text("4000001\n", encoding="utf-8")
    assert not daemon_is_running(tmp_path)


def test_live_pid_reports_running(tmp_path: Path) -> None:
    pid_dir = tmp_path / "daemon"
    pid_dir.mkdir(parents=True)
    (pid_dir / "daemon.pid").write_text(f"{os.getpid()}\n", encoding="utf-8")
    assert daemon_is_running(tmp_path)


def test_stop_running_when_nothing_exists(tmp_path: Path) -> None:
    assert stop_running(tmp_path) is False


def test_stop_running_with_stale_pid_cleans_up(tmp_path: Path) -> None:
    pid_dir = tmp_path / "daemon"
    pid_dir.mkdir(parents=True)
    (pid_dir / "daemon.pid").write_text("4000001\n", encoding="utf-8")
    assert stop_running(tmp_path) is False  # nothing was actually stopped
    # Stale file should be cleaned up.
    assert not (pid_dir / "daemon.pid").exists()


# ── spawn_argv: standalone-binary vs pip-install detection ──────────


@pytest.mark.parametrize(
    ("executable_name", "expects_dash_m"),
    [
        ("python", True),
        ("python3", True),
        ("python3.12", True),
        ("pypy3", True),
        # Standalone binaries: shipyard / sy / anything non-python.
        ("shipyard", False),
        ("sy", False),
        # PyInstaller-style onefile bundle often copies to a temp name.
        ("shipyard-macos-arm64", False),
    ],
)
def test_spawn_argv_detects_install_shape(
    monkeypatch: pytest.MonkeyPatch,
    executable_name: str,
    expects_dash_m: bool,
) -> None:
    """Regression for the 0.22.x daemon.log spam:

        Error: No such option: -m

    Standalone-binary installs (shiv/zipapp) must NOT get ``-m shipyard``
    in the argv — the CLI's Click root rejects it. Pip installs must.
    """
    monkeypatch.setattr("sys.executable", f"/fake/path/{executable_name}")
    argv = _spawn_argv()
    assert argv[0] == f"/fake/path/{executable_name}"
    if expects_dash_m:
        assert argv[1:4] == ["-m", "shipyard", "daemon"]
    else:
        assert "-m" not in argv
        assert argv[1:3] == ["daemon", "run"]

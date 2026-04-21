"""Runner / lifecycle tests — daemon PID semantics + stop_running."""

from __future__ import annotations

import os
from pathlib import Path

from shipyard.daemon.runner import daemon_is_running, stop_running


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

"""Tests for SSH POSIX and Windows executors."""

from __future__ import annotations

import subprocess
from unittest.mock import MagicMock, mock_open, patch

import pytest

from shipyard.core.job import TargetStatus
from shipyard.executor.ssh import SSHExecutor
from shipyard.executor.ssh_windows import SSHWindowsExecutor


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

def _target_config(
    host: str = "ubuntu",
    platform: str = "linux-x64",
    name: str = "ubuntu",
    **extra: object,
) -> dict:
    return {"host": host, "platform": platform, "name": name, **extra}


def _windows_target_config(
    host: str = "win",
    platform: str = "windows-x64",
    name: str = "windows",
    **extra: object,
) -> dict:
    return {
        "host": host,
        "platform": platform,
        "name": name,
        "repo_path": "C:\\repo",
        **extra,
    }


def _validation_config(command: str = "make test") -> dict:
    return {"command": command}


def _mock_bundle_success():
    """Patch all bundle operations to succeed."""
    from shipyard.bundle.git_bundle import BundleResult

    return [
        patch(
            "shipyard.executor.ssh.create_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ),
        patch(
            "shipyard.executor.ssh.upload_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ),
        patch(
            "shipyard.executor.ssh.apply_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ),
    ]


def _mock_windows_bundle_success():
    """Patch bundle operations for Windows executor."""
    from shipyard.bundle.git_bundle import BundleResult

    return [
        patch(
            "shipyard.executor.ssh_windows.create_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ),
        patch(
            "shipyard.executor.ssh_windows.upload_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ),
        patch(
            "shipyard.executor.ssh_windows._apply_bundle_windows",
            return_value=SSHWindowsExecutor.__module__
            and type("R", (), {"success": True, "message": "ok"})(),
        ),
    ]


# ---------------------------------------------------------------------------
# SSHExecutor tests
# ---------------------------------------------------------------------------

class TestSSHExecutorProbe:
    def test_probe_success(self) -> None:
        executor = SSHExecutor()
        mock_result = MagicMock(returncode=0)
        with patch("subprocess.run", return_value=mock_result) as mock_run:
            assert executor.probe(_target_config()) is True
            args = mock_run.call_args
            cmd = args[0][0]
            assert "ssh" in cmd
            assert "ubuntu" in cmd
            assert "echo ok" in " ".join(cmd)

    def test_probe_failure(self) -> None:
        executor = SSHExecutor()
        mock_result = MagicMock(returncode=255)
        with patch("subprocess.run", return_value=mock_result):
            assert executor.probe(_target_config()) is False

    def test_probe_timeout(self) -> None:
        executor = SSHExecutor()
        with patch("subprocess.run", side_effect=subprocess.TimeoutExpired("ssh", 10)):
            assert executor.probe(_target_config()) is False

    def test_probe_os_error(self) -> None:
        executor = SSHExecutor()
        with patch("subprocess.run", side_effect=OSError("no ssh")):
            assert executor.probe(_target_config()) is False

    def test_probe_no_host(self) -> None:
        executor = SSHExecutor()
        assert executor.probe({"platform": "linux-x64"}) is False

    def test_probe_includes_connect_timeout(self) -> None:
        executor = SSHExecutor()
        mock_result = MagicMock(returncode=0)
        with patch("subprocess.run", return_value=mock_result) as mock_run:
            executor.probe(_target_config())
            cmd = mock_run.call_args[0][0]
            assert "-o" in cmd
            idx = cmd.index("-o")
            assert cmd[idx + 1] == "ConnectTimeout=5"


class TestSSHExecutorValidate:
    def test_validate_pass(self, tmp_path) -> None:
        executor = SSHExecutor()
        log_path = str(tmp_path / "log.txt")
        mock_result = MagicMock(returncode=0)

        patches = _mock_bundle_success()
        with patches[0], patches[1], patches[2], \
             patch("subprocess.run", return_value=mock_result):
            result = executor.validate(
                sha="abc123",
                branch="main",
                target_config=_target_config(),
                validation_config=_validation_config(),
                log_path=log_path,
            )

        assert result.status == TargetStatus.PASS
        assert result.backend == "ssh"
        assert result.target_name == "ubuntu"

    def test_validate_fail(self, tmp_path) -> None:
        executor = SSHExecutor()
        log_path = str(tmp_path / "log.txt")
        mock_result = MagicMock(returncode=1)

        patches = _mock_bundle_success()
        with patches[0], patches[1], patches[2], \
             patch("subprocess.run", return_value=mock_result):
            result = executor.validate(
                sha="abc123",
                branch="main",
                target_config=_target_config(),
                validation_config=_validation_config(),
                log_path=log_path,
            )

        assert result.status == TargetStatus.FAIL

    def test_validate_bundle_create_failure(self, tmp_path) -> None:
        from shipyard.bundle.git_bundle import BundleResult

        executor = SSHExecutor()
        log_path = str(tmp_path / "log.txt")

        with patch(
            "shipyard.executor.ssh.create_bundle",
            return_value=BundleResult(success=False, message="git not found"),
        ):
            result = executor.validate(
                sha="abc123",
                branch="main",
                target_config=_target_config(),
                validation_config=_validation_config(),
                log_path=log_path,
            )

        assert result.status == TargetStatus.ERROR
        assert "Bundle creation failed" in (result.error_message or "")

    def test_validate_timeout(self, tmp_path) -> None:
        executor = SSHExecutor()
        log_path = str(tmp_path / "log.txt")

        patches = _mock_bundle_success()
        with patches[0], patches[1], patches[2], \
             patch("subprocess.run", side_effect=subprocess.TimeoutExpired("ssh", 1800)):
            result = executor.validate(
                sha="abc123",
                branch="main",
                target_config=_target_config(),
                validation_config=_validation_config(),
                log_path=log_path,
            )

        assert result.status == TargetStatus.ERROR
        assert "timed out" in (result.error_message or "").lower()

    def test_validate_no_command(self, tmp_path) -> None:
        executor = SSHExecutor()
        log_path = str(tmp_path / "log.txt")

        patches = _mock_bundle_success()
        with patches[0], patches[1], patches[2]:
            result = executor.validate(
                sha="abc123",
                branch="main",
                target_config=_target_config(),
                validation_config={},
                log_path=log_path,
            )

        assert result.status == TargetStatus.ERROR
        assert "No validation command" in (result.error_message or "")

    def test_validate_uses_step_commands(self, tmp_path) -> None:
        executor = SSHExecutor()
        log_path = str(tmp_path / "log.txt")
        mock_result = MagicMock(returncode=0)

        patches = _mock_bundle_success()
        with patches[0], patches[1], patches[2], \
             patch("subprocess.run", return_value=mock_result) as mock_run:
            executor.validate(
                sha="abc123",
                branch="main",
                target_config=_target_config(),
                validation_config={"build": "make", "test": "make test"},
                log_path=log_path,
            )

            # The SSH command should include the chained build + test
            ssh_cmd = mock_run.call_args[0][0]
            remote_cmd = ssh_cmd[-1]
            assert "make && make test" in remote_cmd


# ---------------------------------------------------------------------------
# SSHWindowsExecutor tests
# ---------------------------------------------------------------------------

class TestSSHWindowsExecutorProbe:
    def test_probe_success(self) -> None:
        executor = SSHWindowsExecutor()
        mock_result = MagicMock(returncode=0)
        with patch("subprocess.run", return_value=mock_result) as mock_run:
            assert executor.probe(_windows_target_config()) is True
            cmd = mock_run.call_args[0][0]
            assert "powershell" in cmd
            assert "Write-Output ok" in " ".join(cmd)

    def test_probe_failure(self) -> None:
        executor = SSHWindowsExecutor()
        mock_result = MagicMock(returncode=1)
        with patch("subprocess.run", return_value=mock_result):
            assert executor.probe(_windows_target_config()) is False

    def test_probe_no_host(self) -> None:
        executor = SSHWindowsExecutor()
        assert executor.probe({"platform": "windows-x64"}) is False


class TestSSHWindowsExecutorValidate:
    def test_validate_pass(self, tmp_path) -> None:
        executor = SSHWindowsExecutor()
        log_path = str(tmp_path / "log.txt")
        mock_result = MagicMock(returncode=0)

        from shipyard.bundle.git_bundle import BundleResult

        with patch(
            "shipyard.executor.ssh_windows.create_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows.upload_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows._apply_bundle_windows",
            return_value=type("R", (), {"success": True, "message": "ok"})(),
        ), patch("subprocess.run", return_value=mock_result):
            result = executor.validate(
                sha="abc123",
                branch="main",
                target_config=_windows_target_config(),
                validation_config=_validation_config(),
                log_path=log_path,
            )

        assert result.status == TargetStatus.PASS
        assert result.backend == "ssh-windows"

    def test_validate_fail(self, tmp_path) -> None:
        executor = SSHWindowsExecutor()
        log_path = str(tmp_path / "log.txt")
        mock_result = MagicMock(returncode=1)

        from shipyard.bundle.git_bundle import BundleResult

        with patch(
            "shipyard.executor.ssh_windows.create_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows.upload_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows._apply_bundle_windows",
            return_value=type("R", (), {"success": True, "message": "ok"})(),
        ), patch("subprocess.run", return_value=mock_result):
            result = executor.validate(
                sha="abc123",
                branch="main",
                target_config=_windows_target_config(),
                validation_config=_validation_config(),
                log_path=log_path,
            )

        assert result.status == TargetStatus.FAIL

    def test_validate_uses_powershell(self, tmp_path) -> None:
        executor = SSHWindowsExecutor()
        log_path = str(tmp_path / "log.txt")
        mock_result = MagicMock(returncode=0)

        from shipyard.bundle.git_bundle import BundleResult

        with patch(
            "shipyard.executor.ssh_windows.create_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows.upload_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows._apply_bundle_windows",
            return_value=type("R", (), {"success": True, "message": "ok"})(),
        ), patch("subprocess.run", return_value=mock_result) as mock_run:
            executor.validate(
                sha="abc123",
                branch="main",
                target_config=_windows_target_config(),
                validation_config=_validation_config(),
                log_path=log_path,
            )

            ssh_cmd = mock_run.call_args[0][0]
            assert "powershell" in ssh_cmd
            assert "-Command" in ssh_cmd

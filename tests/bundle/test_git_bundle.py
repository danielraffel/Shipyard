"""Tests for git bundle operations."""

from __future__ import annotations

import subprocess
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from shipyard.bundle.git_bundle import (
    BundleResult,
    apply_bundle,
    create_bundle,
    upload_bundle,
)


class TestCreateBundle:
    def test_create_success(self, tmp_path: Path) -> None:
        output = tmp_path / "test.bundle"
        mock_result = MagicMock(returncode=0, stderr="")
        with patch("subprocess.run", return_value=mock_result) as mock_run:
            result = create_bundle(sha="abc123", output_path=output)

        assert result.success
        assert result.path == str(output)
        cmd = mock_run.call_args[0][0]
        assert cmd[0] == "git"
        assert "bundle" in cmd
        assert "create" in cmd
        assert "abc123" in cmd

    def test_create_failure(self, tmp_path: Path) -> None:
        output = tmp_path / "test.bundle"
        mock_result = MagicMock(returncode=128, stderr="fatal: bad revision")
        with patch("subprocess.run", return_value=mock_result):
            result = create_bundle(sha="bad_sha", output_path=output)

        assert not result.success
        assert "bad revision" in result.message

    def test_create_timeout(self, tmp_path: Path) -> None:
        output = tmp_path / "test.bundle"
        with patch("subprocess.run", side_effect=subprocess.TimeoutExpired("git", 120)):
            result = create_bundle(sha="abc", output_path=output)

        assert not result.success
        assert "timed out" in result.message

    def test_create_os_error(self, tmp_path: Path) -> None:
        output = tmp_path / "test.bundle"
        with patch("subprocess.run", side_effect=OSError("git not found")):
            result = create_bundle(sha="abc", output_path=output)

        assert not result.success
        assert "OS error" in result.message

    def test_create_with_repo_dir(self, tmp_path: Path) -> None:
        output = tmp_path / "test.bundle"
        repo = tmp_path / "myrepo"
        mock_result = MagicMock(returncode=0, stderr="")
        with patch("subprocess.run", return_value=mock_result) as mock_run:
            create_bundle(sha="abc", output_path=output, repo_dir=repo)

        assert mock_run.call_args[1]["cwd"] == str(repo)

    def test_create_makes_parent_dirs(self, tmp_path: Path) -> None:
        output = tmp_path / "deep" / "nested" / "test.bundle"
        mock_result = MagicMock(returncode=0, stderr="")
        with patch("subprocess.run", return_value=mock_result):
            result = create_bundle(sha="abc", output_path=output)

        assert result.success
        assert (tmp_path / "deep" / "nested").is_dir()


class TestUploadBundle:
    def test_upload_success(self, tmp_path: Path) -> None:
        bundle = tmp_path / "test.bundle"
        bundle.write_bytes(b"fake bundle")
        mock_result = MagicMock(returncode=0, stderr="")
        with patch("subprocess.run", return_value=mock_result) as mock_run:
            result = upload_bundle(
                bundle_path=bundle,
                host="ubuntu",
                remote_path="/tmp/test.bundle",
            )

        assert result.success
        assert result.path == "/tmp/test.bundle"
        cmd = mock_run.call_args[0][0]
        # Upload uses ssh+cat instead of scp to avoid SFTP hang
        assert cmd[0] == "ssh"
        assert "ubuntu" in cmd
        assert "cat > /tmp/test.bundle" in " ".join(cmd)

    def test_upload_file_not_found(self, tmp_path: Path) -> None:
        result = upload_bundle(
            bundle_path=tmp_path / "nonexistent.bundle",
            host="ubuntu",
            remote_path="/tmp/test.bundle",
        )
        assert not result.success
        assert "not found" in result.message

    def test_upload_scp_failure(self, tmp_path: Path) -> None:
        bundle = tmp_path / "test.bundle"
        bundle.write_bytes(b"fake")
        mock_result = MagicMock(returncode=1, stderr="Permission denied")
        with patch("subprocess.run", return_value=mock_result):
            result = upload_bundle(
                bundle_path=bundle,
                host="ubuntu",
                remote_path="/tmp/test.bundle",
            )

        assert not result.success
        assert "Permission denied" in result.message

    def test_upload_timeout(self, tmp_path: Path) -> None:
        bundle = tmp_path / "test.bundle"
        bundle.write_bytes(b"fake")
        with patch("subprocess.run", side_effect=subprocess.TimeoutExpired("scp", 300)):
            result = upload_bundle(
                bundle_path=bundle,
                host="ubuntu",
                remote_path="/tmp/test.bundle",
            )

        assert not result.success
        assert "timed out" in result.message

    def test_upload_with_ssh_options(self, tmp_path: Path) -> None:
        bundle = tmp_path / "test.bundle"
        bundle.write_bytes(b"fake")
        mock_result = MagicMock(returncode=0, stderr="")
        with patch("subprocess.run", return_value=mock_result) as mock_run:
            upload_bundle(
                bundle_path=bundle,
                host="ubuntu",
                remote_path="/tmp/test.bundle",
                ssh_options=["-o", "StrictHostKeyChecking=no"],
            )

        cmd = mock_run.call_args[0][0]
        assert "-o" in cmd
        assert "StrictHostKeyChecking=no" in cmd


class TestApplyBundle:
    def test_apply_success(self) -> None:
        mock_result = MagicMock(returncode=0, stderr="")
        with patch("subprocess.run", return_value=mock_result) as mock_run:
            result = apply_bundle(
                host="ubuntu",
                bundle_path="/tmp/test.bundle",
                repo_path="/home/user/repo",
            )

        assert result.success
        cmd = mock_run.call_args[0][0]
        assert cmd[0] == "ssh"
        remote_cmd = cmd[-1]
        assert "bundle verify" in remote_cmd
        assert "git fetch" in remote_cmd

    def test_apply_failure(self) -> None:
        mock_result = MagicMock(returncode=1, stderr="not a bundle")
        with patch("subprocess.run", return_value=mock_result):
            result = apply_bundle(
                host="ubuntu",
                bundle_path="/tmp/bad.bundle",
                repo_path="/home/user/repo",
            )

        assert not result.success
        assert "not a bundle" in result.message

    def test_apply_timeout(self) -> None:
        with patch("subprocess.run", side_effect=subprocess.TimeoutExpired("ssh", 120)):
            result = apply_bundle(
                host="ubuntu",
                bundle_path="/tmp/test.bundle",
                repo_path="/home/user/repo",
            )

        assert not result.success
        assert "timed out" in result.message

    def test_apply_with_ssh_options(self) -> None:
        mock_result = MagicMock(returncode=0, stderr="")
        with patch("subprocess.run", return_value=mock_result) as mock_run:
            apply_bundle(
                host="ubuntu",
                bundle_path="/tmp/test.bundle",
                repo_path="/home/user/repo",
                ssh_options=["-i", "/path/to/key"],
            )

        cmd = mock_run.call_args[0][0]
        assert "-i" in cmd
        assert "/path/to/key" in cmd

    def test_apply_os_error(self) -> None:
        with patch("subprocess.run", side_effect=OSError("ssh not found")):
            result = apply_bundle(
                host="ubuntu",
                bundle_path="/tmp/test.bundle",
                repo_path="/home/user/repo",
            )

        assert not result.success
        assert "OS error" in result.message


class TestBundleResult:
    def test_frozen_dataclass(self) -> None:
        r = BundleResult(success=True, message="ok")
        with pytest.raises(AttributeError):
            r.success = False  # type: ignore[misc]

    def test_optional_path(self) -> None:
        r = BundleResult(success=True, message="ok")
        assert r.path is None
        r2 = BundleResult(success=True, message="ok", path="/tmp/b")
        assert r2.path == "/tmp/b"

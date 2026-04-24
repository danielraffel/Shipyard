"""Tests for SSH POSIX and Windows executors."""

from __future__ import annotations

import subprocess
from datetime import datetime, timezone
from unittest.mock import MagicMock, patch

from shipyard.core.job import TargetStatus
from shipyard.executor.ssh import SSHExecutor
from shipyard.executor.ssh_windows import (
    SSHWindowsExecutor,
    _BundleProbe,
    decode_encoded_ssh_argv,
)
from shipyard.executor.streaming import StreamingCommandResult


def _ok_probe() -> _BundleProbe:
    # Default passing probe for ssh_windows validate() tests. Real
    # subprocess-level coverage lives in test_247_bundle_post_upload_probe.py;
    # existing validate-flow tests only need a "probe said the file
    # is there" stub so they don't make real SSH calls to mock hosts.
    return _BundleProbe(
        exists=True, size=1024,
        detail="OK size=1024 mtime=2026-01-01T00:00:00Z path=test.bundle",
    )

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
    # `windows_vs_detect` defaults to True in production, but tests
    # that don't exercise toolchain detection disable it so they
    # don't shell out to a real `ssh <host>` during unit runs.
    return {
        "host": host,
        "platform": platform,
        "name": name,
        "repo_path": "C:\\repo",
        "windows_vs_detect": False,
        "windows_host_mutex": False,
        **extra,
    }


def _validation_config(command: str = "make test") -> dict:
    return {"command": command}


def _streaming_result(returncode: int = 0, output: str = "") -> StreamingCommandResult:
    now = datetime.now(timezone.utc)
    return StreamingCommandResult(
        returncode=returncode,
        output=output,
        started_at=now,
        completed_at=now,
        duration_secs=1.0,
        last_output_at=now if output else None,
        phase="test" if output else None,
    )


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
            "shipyard.executor.ssh_windows._probe_remote_bundle",
            return_value=_ok_probe(),
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

        patches = _mock_bundle_success()
        with patches[0], patches[1], patches[2], \
             patch("shipyard.executor.ssh.run_streaming_command", return_value=_streaming_result(0, "ok")):
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
        patches = _mock_bundle_success()
        with patches[0], patches[1], patches[2], \
             patch("shipyard.executor.ssh.run_streaming_command", return_value=_streaming_result(1, "failed")):
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
             patch(
                 "shipyard.executor.ssh.run_streaming_command",
                 side_effect=subprocess.TimeoutExpired("ssh", 1800),
             ):
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
        patches = _mock_bundle_success()
        with patches[0], patches[1], patches[2], \
             patch(
                 "shipyard.executor.ssh.run_streaming_command",
                 return_value=_streaming_result(0, "ok"),
             ) as mock_run:
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
            assert "__SHIPYARD_PHASE__:build" in remote_cmd
            assert "__SHIPYARD_PHASE__:test" in remote_cmd
            assert "make test" in remote_cmd


class TestSSHRemoteHeadSha:
    """Tests for _remote_head_sha used in incremental bundle negotiation."""

    def test_returns_sha_on_success(self) -> None:
        from shipyard.executor.ssh import _remote_head_sha
        mock_result = MagicMock(returncode=0, stdout="abc123def456\n", stderr="")
        with patch("subprocess.run", return_value=mock_result):
            sha = _remote_head_sha("ubuntu", "~/repo", [])
        assert sha == "abc123def456"

    def test_returns_none_on_failure(self) -> None:
        from shipyard.executor.ssh import _remote_head_sha
        mock_result = MagicMock(returncode=128, stdout="", stderr="not a repo")
        with patch("subprocess.run", return_value=mock_result):
            sha = _remote_head_sha("ubuntu", "~/repo", [])
        assert sha is None

    def test_returns_none_on_timeout(self) -> None:
        from shipyard.executor.ssh import _remote_head_sha
        with patch("subprocess.run", side_effect=subprocess.TimeoutExpired("ssh", 15)):
            sha = _remote_head_sha("ubuntu", "~/repo", [])
        assert sha is None

    def test_returns_none_on_non_hex_output(self) -> None:
        from shipyard.executor.ssh import _remote_head_sha
        mock_result = MagicMock(returncode=0, stdout="not-a-sha\n", stderr="")
        with patch("subprocess.run", return_value=mock_result):
            sha = _remote_head_sha("ubuntu", "~/repo", [])
        assert sha is None

    def test_returns_none_on_empty_output(self) -> None:
        from shipyard.executor.ssh import _remote_head_sha
        mock_result = MagicMock(returncode=0, stdout="", stderr="")
        with patch("subprocess.run", return_value=mock_result):
            sha = _remote_head_sha("ubuntu", "~/repo", [])
        assert sha is None


class TestSSHIncrementalBundle:
    """Tests for incremental bundle negotiation in SSH executor."""

    def test_uses_basis_sha_when_remote_has_head(self, tmp_path) -> None:
        from shipyard.bundle.git_bundle import BundleResult

        executor = SSHExecutor()
        log_path = str(tmp_path / "log.txt")

        with patch(
            "shipyard.executor.ssh._remote_has_sha", return_value=False,
        ), patch(
            "shipyard.executor.ssh._remote_head_sha", return_value="aabbcc",
        ), patch(
            "shipyard.executor.ssh._local_has_commit", return_value=True,
        ), patch(
            "shipyard.executor.ssh.create_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ) as mock_create, patch(
            "shipyard.executor.ssh.upload_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh.apply_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh.run_streaming_command",
            return_value=_streaming_result(0, "ok"),
        ):
            executor.validate(
                sha="abc123", branch="main",
                target_config=_target_config(),
                validation_config=_validation_config(),
                log_path=log_path,
            )

        # First call should use basis_shas
        first_call = mock_create.call_args_list[0]
        assert first_call[1].get("basis_shas") == ["aabbcc"]

    def test_falls_back_to_full_bundle_on_incremental_failure(self, tmp_path) -> None:
        from shipyard.bundle.git_bundle import BundleResult

        executor = SSHExecutor()
        log_path = str(tmp_path / "log.txt")

        create_results = [
            BundleResult(success=False, message="bad basis"),  # incremental fails
            BundleResult(success=True, message="ok", path="/tmp/b"),  # full succeeds
        ]

        with patch(
            "shipyard.executor.ssh._remote_has_sha", return_value=False,
        ), patch(
            "shipyard.executor.ssh._remote_head_sha", return_value="aabbcc",
        ), patch(
            "shipyard.executor.ssh._local_has_commit", return_value=True,
        ), patch(
            "shipyard.executor.ssh.create_bundle",
            side_effect=create_results,
        ) as mock_create, patch(
            "shipyard.executor.ssh.upload_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh.apply_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh.run_streaming_command",
            return_value=_streaming_result(0, "ok"),
        ):
            result = executor.validate(
                sha="abc123", branch="main",
                target_config=_target_config(),
                validation_config=_validation_config(),
                log_path=log_path,
            )

        assert result.status == TargetStatus.PASS
        assert mock_create.call_count == 2
        # Second call should NOT have basis_shas (full bundle)
        second_call = mock_create.call_args_list[1]
        assert not second_call[1].get("basis_shas")

    def test_skips_negotiation_when_remote_head_unknown(self, tmp_path) -> None:
        from shipyard.bundle.git_bundle import BundleResult

        executor = SSHExecutor()
        log_path = str(tmp_path / "log.txt")

        with patch(
            "shipyard.executor.ssh._remote_has_sha", return_value=False,
        ), patch(
            "shipyard.executor.ssh._remote_head_sha", return_value=None,
        ), patch(
            "shipyard.executor.ssh._local_has_commit", return_value=False,
        ), patch(
            "shipyard.executor.ssh.create_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ) as mock_create, patch(
            "shipyard.executor.ssh.upload_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh.apply_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh.run_streaming_command",
            return_value=_streaming_result(0, "ok"),
        ):
            executor.validate(
                sha="abc123", branch="main",
                target_config=_target_config(),
                validation_config=_validation_config(),
                log_path=log_path,
            )

        # Should create a full bundle (no basis)
        assert mock_create.call_count == 1
        first_call = mock_create.call_args_list[0]
        assert not first_call[1].get("basis_shas")

    def test_skips_basis_when_local_missing_ancestor(self, tmp_path) -> None:
        """Remote HEAD is known but not present locally → full bundle.

        This is the case the issue calls out: the ancestry check must
        reject basis SHAs the local clone hasn't fetched, otherwise
        `git bundle create ^<basis>` silently degenerates into a full
        bundle (or fails) and wastes the delta attempt.
        """
        from shipyard.bundle.git_bundle import BundleResult

        executor = SSHExecutor()
        log_path = str(tmp_path / "log.txt")

        with patch(
            "shipyard.executor.ssh._remote_has_sha", return_value=False,
        ), patch(
            "shipyard.executor.ssh._remote_head_sha", return_value="deadbeef",
        ), patch(
            "shipyard.executor.ssh._local_has_commit", return_value=False,
        ), patch(
            "shipyard.executor.ssh.create_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ) as mock_create, patch(
            "shipyard.executor.ssh.upload_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh.apply_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh.run_streaming_command",
            return_value=_streaming_result(0, "ok"),
        ):
            executor.validate(
                sha="abc123", branch="main",
                target_config=_target_config(),
                validation_config=_validation_config(),
                log_path=log_path,
            )

        assert mock_create.call_count == 1
        first_call = mock_create.call_args_list[0]
        assert not first_call[1].get("basis_shas")

    def test_logs_bundle_mode_and_bytes_for_delta(self, tmp_path) -> None:
        """Delta path must log bundle_mode=delta and bundle_bytes=<N>."""
        from shipyard.bundle.git_bundle import BundleResult

        executor = SSHExecutor()
        log_path = tmp_path / "log.txt"

        def fake_create_bundle(sha, output_path, repo_dir=None, basis_shas=()):
            # Simulate the bundle writer so _safe_filesize returns a
            # realistic delta size (few KB rather than 443 MB).
            from pathlib import Path as _P
            _P(output_path).write_bytes(b"DELTA" * 200)
            return BundleResult(success=True, message="ok", path=str(output_path))

        with patch(
            "shipyard.executor.ssh._remote_has_sha", return_value=False,
        ), patch(
            "shipyard.executor.ssh._remote_head_sha", return_value="aabbccddeeff",
        ), patch(
            "shipyard.executor.ssh._local_has_commit", return_value=True,
        ), patch(
            "shipyard.executor.ssh.create_bundle",
            side_effect=fake_create_bundle,
        ), patch(
            "shipyard.executor.ssh.upload_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh.apply_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh.run_streaming_command",
            return_value=_streaming_result(0, "ok"),
        ):
            executor.validate(
                sha="abc123", branch="main",
                target_config=_target_config(),
                validation_config=_validation_config(),
                log_path=str(log_path),
            )

        contents = log_path.read_text()
        assert "bundle_mode=delta" in contents
        assert "bundle_bytes=1000" in contents
        assert "remote_head=aabbccddeeff" in contents

    def test_logs_bundle_mode_full_when_no_basis(self, tmp_path) -> None:
        """Full path must log bundle_mode=full and the byte count."""
        from shipyard.bundle.git_bundle import BundleResult

        executor = SSHExecutor()
        log_path = tmp_path / "log.txt"

        def fake_create_bundle(sha, output_path, repo_dir=None, basis_shas=()):
            from pathlib import Path as _P
            _P(output_path).write_bytes(b"FULLBUNDLE" * 10)
            return BundleResult(success=True, message="ok", path=str(output_path))

        with patch(
            "shipyard.executor.ssh._remote_has_sha", return_value=False,
        ), patch(
            "shipyard.executor.ssh._remote_head_sha", return_value=None,
        ), patch(
            "shipyard.executor.ssh._local_has_commit", return_value=False,
        ), patch(
            "shipyard.executor.ssh.create_bundle",
            side_effect=fake_create_bundle,
        ), patch(
            "shipyard.executor.ssh.upload_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh.apply_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh.run_streaming_command",
            return_value=_streaming_result(0, "ok"),
        ):
            executor.validate(
                sha="abc123", branch="main",
                target_config=_target_config(),
                validation_config=_validation_config(),
                log_path=str(log_path),
            )

        contents = log_path.read_text()
        assert "bundle_mode=full" in contents
        assert "bundle_bytes=100" in contents
        assert "remote_head=unknown" in contents


class TestLocalHasCommit:
    """Tests for the ancestry probe used to validate basis SHAs."""

    def test_returns_true_when_commit_exists(self) -> None:
        from shipyard.executor.ssh import _local_has_commit
        mock_result = MagicMock(returncode=0, stdout="", stderr="")
        with patch("subprocess.run", return_value=mock_result) as mock_run:
            assert _local_has_commit("abc123") is True
            cmd = mock_run.call_args[0][0]
            assert cmd[0] == "git"
            assert "cat-file" in cmd
            # Must query the commit type specifically (prevents
            # false positives on dangling blobs / trees).
            assert any(arg.startswith("abc123") and "commit" in arg for arg in cmd)

    def test_returns_false_when_commit_missing(self) -> None:
        from shipyard.executor.ssh import _local_has_commit
        mock_result = MagicMock(returncode=1, stdout="", stderr="not found")
        with patch("subprocess.run", return_value=mock_result):
            assert _local_has_commit("deadbeef") is False

    def test_returns_false_on_timeout(self) -> None:
        from shipyard.executor.ssh import _local_has_commit
        with patch(
            "subprocess.run",
            side_effect=subprocess.TimeoutExpired("git", 10),
        ):
            assert _local_has_commit("abc123") is False

    def test_returns_false_on_os_error(self) -> None:
        from shipyard.executor.ssh import _local_has_commit
        with patch("subprocess.run", side_effect=OSError("no git")):
            assert _local_has_commit("abc123") is False

    def test_uses_repo_dir_cwd(self, tmp_path) -> None:
        from shipyard.executor.ssh import _local_has_commit
        mock_result = MagicMock(returncode=0, stdout="", stderr="")
        with patch("subprocess.run", return_value=mock_result) as mock_run:
            _local_has_commit("abc123", repo_dir=str(tmp_path))
            assert mock_run.call_args[1]["cwd"] == str(tmp_path)


class TestSSHResumeFrom:
    """Tests for --resume-from support in SSH executor."""

    def _stage_validation_config(self) -> dict:
        return {
            "setup": "echo setup",
            "configure": "cmake .",
            "build": "cmake --build .",
            "test": "ctest",
        }

    def test_build_command_includes_markers_per_stage(self) -> None:
        from shipyard.executor.ssh import _build_remote_command
        cfg = self._stage_validation_config()
        cmd = _build_remote_command("abc123def456", "/repo", cfg)
        assert cmd is not None
        # Each stage should write its own marker
        assert ".shipyard-stage-setup-abc123def456" in cmd
        assert ".shipyard-stage-configure-abc123def456" in cmd
        assert ".shipyard-stage-build-abc123def456" in cmd
        assert ".shipyard-stage-test-abc123def456" in cmd

    def test_build_command_respects_resume_from(self) -> None:
        from shipyard.executor.ssh import _build_remote_command
        cfg = self._stage_validation_config()
        cmd = _build_remote_command(
            "abc123def456", "/repo", cfg, resume_from="test",
        )
        assert cmd is not None
        # Earlier stages should be skipped
        assert "echo setup" not in cmd
        assert "cmake ." not in cmd
        assert "cmake --build" not in cmd
        # test stage and its marker should be present
        assert "ctest" in cmd
        assert ".shipyard-stage-test-" in cmd

    def test_resume_from_honored_when_marker_exists(self, tmp_path) -> None:
        from shipyard.executor.ssh import _resolve_resume_from
        log = tmp_path / "log.txt"
        with patch(
            "shipyard.executor.ssh._remote_marker_exists", return_value=True,
        ):
            result = _resolve_resume_from(
                host="ubuntu", remote_repo="/repo", sha="abc123",
                ssh_options=[], requested="test",
                validation_config=self._stage_validation_config(),
                log_file=log,
            )
        assert result == "test"

    def test_resume_from_falls_back_when_marker_missing(self, tmp_path) -> None:
        from shipyard.executor.ssh import _resolve_resume_from
        log = tmp_path / "log.txt"
        with patch(
            "shipyard.executor.ssh._remote_marker_exists", return_value=False,
        ):
            result = _resolve_resume_from(
                host="ubuntu", remote_repo="/repo", sha="abc123",
                ssh_options=[], requested="test",
                validation_config=self._stage_validation_config(),
                log_file=log,
            )
        assert result is None
        assert "marker for previous stage" in log.read_text()

    def test_resume_from_ignored_for_single_command(self, tmp_path) -> None:
        from shipyard.executor.ssh import _resolve_resume_from
        log = tmp_path / "log.txt"
        result = _resolve_resume_from(
            host="ubuntu", remote_repo="/repo", sha="abc123",
            ssh_options=[], requested="test",
            validation_config={"command": "make test"},
            log_file=log,
        )
        assert result is None
        assert "single command" in log.read_text()

    def test_resume_from_none_returns_none(self, tmp_path) -> None:
        from shipyard.executor.ssh import _resolve_resume_from
        log = tmp_path / "log.txt"
        result = _resolve_resume_from(
            host="ubuntu", remote_repo="/repo", sha="abc123",
            ssh_options=[], requested=None,
            validation_config=self._stage_validation_config(),
            log_file=log,
        )
        assert result is None

    def test_validate_passes_resume_from_through(self, tmp_path) -> None:

        executor = SSHExecutor()
        log_path = str(tmp_path / "log.txt")
        cfg = self._stage_validation_config()

        with patch(
            "shipyard.executor.ssh._remote_has_sha", return_value=True,
        ), patch(
            "shipyard.executor.ssh._remote_marker_exists", return_value=True,
        ), patch(
            "shipyard.executor.ssh.run_streaming_command",
            return_value=_streaming_result(0, "ok"),
        ) as mock_run:
            executor.validate(
                sha="abc123def456", branch="main",
                target_config=_target_config(),
                validation_config=cfg,
                log_path=log_path,
                resume_from="test",
            )

        ssh_cmd = mock_run.call_args[0][0]
        remote_cmd = ssh_cmd[-1]
        assert "ctest" in remote_cmd
        assert "echo setup" not in remote_cmd


# ---------------------------------------------------------------------------
# SSHWindowsExecutor tests
# ---------------------------------------------------------------------------

class TestSSHWindowsExecutorProbe:
    def test_probe_success(self) -> None:
        executor = SSHWindowsExecutor()
        mock_result = MagicMock(returncode=0, stdout="ok\n", stderr="")
        with patch("subprocess.run", return_value=mock_result) as mock_run:
            assert executor.probe(_windows_target_config()) is True
            cmd = mock_run.call_args[0][0]
            # #119: probe now uses `echo ok` (cross-shell — cmd.exe, PowerShell,
            # bash all accept it) + BatchMode=yes. The prior `powershell
            # -Command Write-Output ok` shape hung on hosts whose default
            # shell was cmd.exe and never surfaced a classified failure.
            assert "echo" in cmd
            assert "BatchMode=yes" in cmd

    def test_probe_failure(self) -> None:
        executor = SSHWindowsExecutor()
        mock_result = MagicMock(returncode=1, stdout="", stderr="Permission denied (publickey).")
        with patch("subprocess.run", return_value=mock_result):
            assert executor.probe(_windows_target_config()) is False

    def test_probe_no_host(self) -> None:
        executor = SSHWindowsExecutor()
        assert executor.probe({"platform": "windows-x64"}) is False


class TestSSHWindowsExecutorValidate:
    def test_validate_pass(self, tmp_path) -> None:
        executor = SSHWindowsExecutor()
        log_path = str(tmp_path / "log.txt")
        from shipyard.bundle.git_bundle import BundleResult

        with patch(
            "shipyard.executor.ssh_windows.create_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows.upload_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows._probe_remote_bundle",
            return_value=_ok_probe(),
        ), patch(
            "shipyard.executor.ssh_windows._apply_bundle_windows",
            return_value=type("R", (), {"success": True, "message": "ok"})(),
        ), patch(
            "shipyard.executor.ssh_windows.run_streaming_command",
            return_value=_streaming_result(0, "ok"),
        ):
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
        from shipyard.bundle.git_bundle import BundleResult

        with patch(
            "shipyard.executor.ssh_windows.create_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows.upload_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows._probe_remote_bundle",
            return_value=_ok_probe(),
        ), patch(
            "shipyard.executor.ssh_windows._apply_bundle_windows",
            return_value=type("R", (), {"success": True, "message": "ok"})(),
        ), patch(
            "shipyard.executor.ssh_windows.run_streaming_command",
            return_value=_streaming_result(1, "failed"),
        ):
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
        from shipyard.bundle.git_bundle import BundleResult

        with patch(
            "shipyard.executor.ssh_windows.create_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows.upload_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows._probe_remote_bundle",
            return_value=_ok_probe(),
        ), patch(
            "shipyard.executor.ssh_windows._apply_bundle_windows",
            return_value=type("R", (), {"success": True, "message": "ok"})(),
        ), patch(
            "shipyard.executor.ssh_windows.run_streaming_command",
            return_value=_streaming_result(0, "ok"),
        ) as mock_run:
            executor.validate(
                sha="abc123",
                branch="main",
                target_config=_windows_target_config(),
                validation_config=_validation_config(),
                log_path=log_path,
            )

            ssh_cmd = mock_run.call_args[0][0]
            assert "powershell" in ssh_cmd
            # The command must be sent via -EncodedCommand, not
            # -Command, because Windows OpenSSH's cmd.exe shell
            # interprets newlines in -Command arguments as command
            # separators and silently drops every line after the
            # first. -EncodedCommand bypasses cmd.exe parsing.
            assert "-EncodedCommand" in ssh_cmd
            assert "-Command" not in ssh_cmd
            assert "-NoProfile" in ssh_cmd

    def test_validate_host_mutex_default_wraps_command(self, tmp_path) -> None:
        """When windows_host_mutex is not explicitly disabled, the command is wrapped."""
        executor = SSHWindowsExecutor()
        log_path = str(tmp_path / "log.txt")
        from shipyard.bundle.git_bundle import BundleResult

        # Enable the mutex but keep VS detection disabled so no real
        # SSH call is made.
        target = _windows_target_config(
            windows_vs_detect=False,
            windows_host_mutex=True,
        )

        with patch(
            "shipyard.executor.ssh_windows.create_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows.upload_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows._probe_remote_bundle",
            return_value=_ok_probe(),
        ), patch(
            "shipyard.executor.ssh_windows._apply_bundle_windows",
            return_value=type("R", (), {"success": True, "message": "ok"})(),
        ), patch(
            "shipyard.executor.ssh_windows.run_streaming_command",
            return_value=_streaming_result(0, "ok"),
        ) as mock_run:
            executor.validate(
                sha="abc123",
                branch="main",
                target_config=target,
                validation_config=_validation_config(),
                log_path=log_path,
            )

            ssh_cmd = mock_run.call_args[0][0]
            # The PS script is base64-encoded inside -EncodedCommand;
            # decode before asserting against the source.
            ps = decode_encoded_ssh_argv(ssh_cmd)
            assert ps is not None
            assert "System.Threading.Mutex" in ps
            assert "Global\\ShipyardValidate" in ps

    def test_validate_host_mutex_custom_name(self, tmp_path) -> None:
        executor = SSHWindowsExecutor()
        log_path = str(tmp_path / "log.txt")
        from shipyard.bundle.git_bundle import BundleResult

        target = _windows_target_config(
            windows_vs_detect=False,
            windows_host_mutex=True,
            windows_host_mutex_name="Global\\MyCustomLock",
        )

        with patch(
            "shipyard.executor.ssh_windows.create_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows.upload_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows._probe_remote_bundle",
            return_value=_ok_probe(),
        ), patch(
            "shipyard.executor.ssh_windows._apply_bundle_windows",
            return_value=type("R", (), {"success": True, "message": "ok"})(),
        ), patch(
            "shipyard.executor.ssh_windows.run_streaming_command",
            return_value=_streaming_result(0, "ok"),
        ) as mock_run:
            executor.validate(
                sha="abc123",
                branch="main",
                target_config=target,
                validation_config=_validation_config(),
                log_path=log_path,
            )
            ps = decode_encoded_ssh_argv(mock_run.call_args[0][0])
            assert ps is not None
            assert "Global\\MyCustomLock" in ps
            assert "Global\\ShipyardValidate" not in ps

    def test_validate_vs_detection_injects_env_vars(self, tmp_path) -> None:
        """Detected toolchain is exported to the remote command as env vars."""
        from shipyard.executor.windows_toolchain import VsToolchain

        executor = SSHWindowsExecutor()
        log_path = str(tmp_path / "log.txt")
        from shipyard.bundle.git_bundle import BundleResult

        target = _windows_target_config(
            windows_vs_detect=True,
            windows_host_mutex=False,
        )

        with patch(
            "shipyard.executor.ssh_windows.detect_vs_toolchain",
            return_value=VsToolchain(
                cmake_platform="ARM64",
                cmake_generator_instance="C:/VS/2022/Community",
            ),
        ), patch(
            "shipyard.executor.ssh_windows.create_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows.upload_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows._probe_remote_bundle",
            return_value=_ok_probe(),
        ), patch(
            "shipyard.executor.ssh_windows._apply_bundle_windows",
            return_value=type("R", (), {"success": True, "message": "ok"})(),
        ), patch(
            "shipyard.executor.ssh_windows.run_streaming_command",
            return_value=_streaming_result(0, "ok"),
        ) as mock_run:
            executor.validate(
                sha="abc123",
                branch="main",
                target_config=target,
                validation_config=_validation_config(),
                log_path=log_path,
            )
            ps = decode_encoded_ssh_argv(mock_run.call_args[0][0])
            assert ps is not None
            assert "$env:SHIPYARD_CMAKE_PLATFORM = 'ARM64'" in ps
            assert "C:/VS/2022/Community" in ps

    def test_validate_vs_detection_cache_reuses_result(self, tmp_path) -> None:
        """The toolchain is detected once per host and cached on the executor."""
        from shipyard.executor.windows_toolchain import VsToolchain

        executor = SSHWindowsExecutor()
        log_path = str(tmp_path / "log.txt")
        from shipyard.bundle.git_bundle import BundleResult

        target = _windows_target_config(
            windows_vs_detect=True,
            windows_host_mutex=False,
        )

        with patch(
            "shipyard.executor.ssh_windows.detect_vs_toolchain",
            return_value=VsToolchain(
                cmake_platform="x64",
                cmake_generator_instance="C:/VS/2022/BuildTools",
            ),
        ) as mock_detect, patch(
            "shipyard.executor.ssh_windows.create_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows.upload_bundle",
            return_value=BundleResult(success=True, message="ok", path="/tmp/b"),
        ), patch(
            "shipyard.executor.ssh_windows._probe_remote_bundle",
            return_value=_ok_probe(),
        ), patch(
            "shipyard.executor.ssh_windows._apply_bundle_windows",
            return_value=type("R", (), {"success": True, "message": "ok"})(),
        ), patch(
            "shipyard.executor.ssh_windows.run_streaming_command",
            return_value=_streaming_result(0, "ok"),
        ):
            for _ in range(3):
                executor.validate(
                    sha="abc123",
                    branch="main",
                    target_config=target,
                    validation_config=_validation_config(),
                    log_path=log_path,
                )
            assert mock_detect.call_count == 1

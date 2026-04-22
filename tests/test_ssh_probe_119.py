"""Regression tests for SSH probe robustness (#119).

The Windows SSH probe previously called `powershell -Command Write-Output ok`
without `BatchMode=yes`, had no retry, and its failures came back with only
a generic "no reachable backend" message because `SSHWindowsExecutor` didn't
implement `diagnose()`. Those gaps meant a reachable Windows host could
report unreachable on a slow handshake and the user had no classified
reason to branch on.

These tests pin the post-fix invariants:
- Both SSH and SSH-Windows probes use `echo ok` (cmd.exe / PowerShell /
  bash all accept it) with BatchMode=yes and ConnectTimeout=5.
- `SSHWindowsExecutor.diagnose()` returns the same shape as
  `SSHExecutor.diagnose()` with stable category strings.
- `run_probe()` retries once on transient (timeout/network) failures but
  not on auth/host_key/configuration.
- `SHIPYARD_DEBUG_PROBE=1` prints the exact probe command.
"""

from __future__ import annotations

import subprocess
from typing import Any

import pytest

from shipyard.executor.ssh import (
    PROBE_CONNECT_TIMEOUT_SECS,
    PROBE_TIMEOUT_SECS,
    _build_probe_cmd,
    run_probe,
)
from shipyard.executor.ssh_windows import SSHWindowsExecutor


# ── Cross-platform probe command invariants ────────────────────────


class TestProbeCommandShape:
    def test_probe_always_includes_batchmode_yes(self) -> None:
        """#119: BatchMode=yes must be in every probe — a missed auth
        prompt was the original failure-to-fail-fast bug."""
        cmd = _build_probe_cmd({"host": "win"}, ["echo", "ok"])
        assert "BatchMode=yes" in cmd

    def test_probe_always_includes_connect_timeout(self) -> None:
        cmd = _build_probe_cmd({"host": "win"}, ["echo", "ok"])
        assert f"ConnectTimeout={PROBE_CONNECT_TIMEOUT_SECS}" in cmd

    def test_probe_uses_argv_form_for_remote_cmd(self) -> None:
        """Remote cmd argv elements are passed individually, not shell-joined,
        so quoting works identically across POSIX/cmd.exe/PowerShell."""
        cmd = _build_probe_cmd({"host": "win"}, ["echo", "ok"])
        # The last two elements must be the remote argv, each its own arg.
        assert cmd[-2:] == ["echo", "ok"]

    def test_probe_carries_configured_ssh_options(self) -> None:
        cmd = _build_probe_cmd(
            {"host": "win", "identity_file": "~/.ssh/win"},
            ["echo", "ok"],
        )
        assert "-i" in cmd
        assert "~/.ssh/win" in cmd


# ── Reachability + classification ──────────────────────────────────


class TestRunProbe:
    def test_reachable_host_returns_reachable_true(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        def _ok(cmd: list[str], **kw: Any) -> subprocess.CompletedProcess:
            return subprocess.CompletedProcess(
                args=cmd, returncode=0, stdout="ok\n", stderr=""
            )

        monkeypatch.setattr(subprocess, "run", _ok)
        diag = run_probe({"host": "win"}, remote_cmd=["echo", "ok"])
        assert diag["reachable"] is True
        assert diag["category"] is None
        assert diag["attempts"] == 1

    def test_auth_failure_is_classified_and_not_retried(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        calls = {"n": 0}

        def _auth_fail(cmd: list[str], **kw: Any) -> subprocess.CompletedProcess:
            calls["n"] += 1
            return subprocess.CompletedProcess(
                args=cmd,
                returncode=255,
                stdout="",
                stderr="Permission denied (publickey).\n",
            )

        monkeypatch.setattr(subprocess, "run", _auth_fail)
        diag = run_probe({"host": "win"}, remote_cmd=["echo", "ok"])
        assert diag["reachable"] is False
        assert diag["category"] == "auth"
        assert diag["attempts"] == 1, "auth is non-transient — must not retry"
        assert calls["n"] == 1

    def test_timeout_is_retried_with_backoff(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """#141: transient timeouts retry with exponential backoff so a
        Tailscale hiccup doesn't abort the ship. Three attempts total.
        Test suppresses the real sleep windows to keep wall-clock
        bounded; the production default is 2s + 6s between attempts."""
        calls = {"n": 0}

        def _timeout(cmd: list[str], **kw: Any) -> subprocess.CompletedProcess:
            calls["n"] += 1
            raise subprocess.TimeoutExpired(cmd=cmd, timeout=kw.get("timeout", 0))

        monkeypatch.setattr(subprocess, "run", _timeout)
        from shipyard.executor import ssh as _ssh
        monkeypatch.setattr(_ssh, "_PROBE_BACKOFFS_SECS", (0.0, 0.0))
        diag = run_probe({"host": "win"}, remote_cmd=["echo", "ok"])
        assert diag["reachable"] is False
        assert diag["category"] == "timeout"
        assert diag["attempts"] == 3
        assert calls["n"] == 3

    def test_network_error_is_retried_with_backoff(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        calls = {"n": 0}

        def _refused(cmd: list[str], **kw: Any) -> subprocess.CompletedProcess:
            calls["n"] += 1
            return subprocess.CompletedProcess(
                args=cmd,
                returncode=255,
                stdout="",
                stderr="ssh: connect to host win port 22: Connection refused\n",
            )

        monkeypatch.setattr(subprocess, "run", _refused)
        from shipyard.executor import ssh as _ssh
        monkeypatch.setattr(_ssh, "_PROBE_BACKOFFS_SECS", (0.0, 0.0))
        diag = run_probe({"host": "win"}, remote_cmd=["echo", "ok"])
        assert diag["reachable"] is False
        assert diag["category"] == "network"
        assert diag["attempts"] == 3

    def test_transient_retry_succeeds_on_second_attempt(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """This is the #119 win: slow Windows SSH handshake times out on
        attempt 1, succeeds on attempt 2. Before the fix the user saw
        unreachable; after the fix they see reachable."""
        calls = {"n": 0}

        def _flaky(cmd: list[str], **kw: Any) -> subprocess.CompletedProcess:
            calls["n"] += 1
            if calls["n"] == 1:
                raise subprocess.TimeoutExpired(
                    cmd=cmd, timeout=kw.get("timeout", 0)
                )
            return subprocess.CompletedProcess(
                args=cmd, returncode=0, stdout="ok\n", stderr=""
            )

        monkeypatch.setattr(subprocess, "run", _flaky)
        diag = run_probe({"host": "win"}, remote_cmd=["echo", "ok"])
        assert diag["reachable"] is True
        assert diag["attempts"] == 2

    def test_missing_host_is_configuration_error(self) -> None:
        diag = run_probe({}, remote_cmd=["echo", "ok"])
        assert diag["reachable"] is False
        assert diag["category"] == "configuration"
        assert diag["attempts"] == 0, "no probe attempted when host missing"


# ── SSHWindowsExecutor alignment with SSHExecutor ─────────────────


class TestWindowsDiagnose:
    def test_diagnose_returns_reachable_schema(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        def _ok(cmd: list[str], **kw: Any) -> subprocess.CompletedProcess:
            return subprocess.CompletedProcess(
                args=cmd, returncode=0, stdout="ok\n", stderr=""
            )

        monkeypatch.setattr(subprocess, "run", _ok)
        diag = SSHWindowsExecutor().diagnose({"host": "win"})
        assert set(diag.keys()) == {"reachable", "message", "category"}
        assert diag["reachable"] is True
        assert diag["category"] is None

    def test_diagnose_classifies_unreachable_same_as_posix(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Same error string should yield the same category on both
        executors so agents can branch on category without caring which
        transport produced it."""
        from shipyard.executor.ssh import SSHExecutor

        def _resolve_fail(cmd: list[str], **kw: Any) -> subprocess.CompletedProcess:
            return subprocess.CompletedProcess(
                args=cmd,
                returncode=255,
                stdout="",
                stderr="ssh: Could not resolve hostname win: Name or service not known\n",
            )

        monkeypatch.setattr(subprocess, "run", _resolve_fail)
        win_diag = SSHWindowsExecutor().diagnose({"host": "win"})
        posix_diag = SSHExecutor().diagnose({"host": "win"})
        assert win_diag["category"] == posix_diag["category"] == "resolution"

    def test_windows_probe_uses_echo_not_powershell(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Regression: the pre-#119 probe invoked `powershell -Command
        Write-Output ok`, which fails when powershell isn't on PATH or
        the default shell is cmd.exe. `echo ok` runs cleanly in cmd.exe,
        PowerShell, and bash alike."""
        captured: list[list[str]] = []

        def _capture(cmd: list[str], **kw: Any) -> subprocess.CompletedProcess:
            captured.append(list(cmd))
            return subprocess.CompletedProcess(
                args=cmd, returncode=0, stdout="ok\n", stderr=""
            )

        monkeypatch.setattr(subprocess, "run", _capture)
        SSHWindowsExecutor().probe({"host": "win"})
        assert len(captured) == 1
        cmd = captured[0]
        assert cmd[-2:] == ["echo", "ok"]
        # Must NOT contain powershell as a bare remote-shell invocation.
        assert "powershell" not in cmd


# ── Debug mode ────────────────────────────────────────────────────


class TestDebugProbeEnvVar:
    def test_debug_flag_prints_exact_command(
        self,
        monkeypatch: pytest.MonkeyPatch,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        def _ok(cmd: list[str], **kw: Any) -> subprocess.CompletedProcess:
            return subprocess.CompletedProcess(
                args=cmd, returncode=0, stdout="ok\n", stderr=""
            )

        monkeypatch.setattr(subprocess, "run", _ok)
        monkeypatch.setenv("SHIPYARD_DEBUG_PROBE", "1")

        run_probe({"host": "win", "name": "windows"}, remote_cmd=["echo", "ok"])
        err = capsys.readouterr().err
        assert "[shipyard:probe]" in err
        assert "target=windows" in err
        assert "echo ok" in err
        assert f"timeout={PROBE_TIMEOUT_SECS}s" in err

    def test_debug_off_is_quiet(
        self,
        monkeypatch: pytest.MonkeyPatch,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        def _ok(cmd: list[str], **kw: Any) -> subprocess.CompletedProcess:
            return subprocess.CompletedProcess(
                args=cmd, returncode=0, stdout="ok\n", stderr=""
            )

        monkeypatch.setattr(subprocess, "run", _ok)
        monkeypatch.delenv("SHIPYARD_DEBUG_PROBE", raising=False)

        run_probe({"host": "win", "name": "windows"}, remote_cmd=["echo", "ok"])
        err = capsys.readouterr().err
        assert "[shipyard:probe]" not in err


# ── #120 secondary: missing host surfaces cleanly, never KeyError ──


class TestMissingHostNeverKeyErrors:
    """#120: if a target's `host` field is missing, both SSH executors
    used to raise `KeyError: 'host'` inside validate(), which surfaced
    as a traceback in the ship flow. Both now return a clean ERROR
    result naming the target and pointing at the config files.
    """

    def test_ssh_validate_missing_host_returns_error_result(
        self, tmp_path: Any
    ) -> None:
        from shipyard.core.job import TargetStatus
        from shipyard.executor.ssh import SSHExecutor

        result = SSHExecutor().validate(
            sha="a" * 40,
            branch="feat/x",
            target_config={"name": "ubuntu", "platform": "linux-x64"},  # no host
            validation_config={"command": "true"},
            log_path=str(tmp_path / "log"),
        )
        assert result.status == TargetStatus.ERROR
        assert "misconfigured" in (result.error_message or "").lower()
        assert "ubuntu" in (result.error_message or "")

    def test_ssh_windows_validate_missing_host_returns_error_result(
        self, tmp_path: Any
    ) -> None:
        from shipyard.core.job import TargetStatus
        from shipyard.executor.ssh_windows import SSHWindowsExecutor

        result = SSHWindowsExecutor().validate(
            sha="a" * 40,
            branch="feat/x",
            target_config={"name": "windows", "platform": "windows-x64"},
            validation_config={"command": "echo ok"},
            log_path=str(tmp_path / "log"),
        )
        assert result.status == TargetStatus.ERROR
        assert "misconfigured" in (result.error_message or "").lower()
        assert "windows" in (result.error_message or "")

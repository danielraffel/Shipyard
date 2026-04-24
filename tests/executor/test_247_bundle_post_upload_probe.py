"""Tests for ssh-windows post-upload bundle probe (#247).

Context: Pulp PR #728 reproduced a persistent ssh-windows bundle-apply
failure against a reachable Windows host. The operator saw a CLIXML-
wrapped `error: could not open 'C:/Users/.../shipyard.bundle'`
interleaved with a PowerShell progress record — i.e. `git` failing
to read a file that `upload_bundle` had just reported as delivered.
The user's suggested fix: verify the uploaded bundle actually landed
(Test-Path + size) before handing off to git.

`_probe_remote_bundle` implements that verification. Tests here drive
it with mocked subprocess output covering every branch the helper has
to return a clean `_BundleProbe` for so `validate()` can distinguish
"upload silently failed" from "apply itself broke."
"""

from __future__ import annotations

import subprocess
from types import SimpleNamespace
from unittest.mock import patch

import pytest  # noqa: TC002 — MonkeyPatch fixture

from shipyard.executor.ssh_windows import _BundleProbe, _probe_remote_bundle


def _proc(
    returncode: int = 0,
    stdout: str = "",
    stderr: str = "",
) -> SimpleNamespace:
    return SimpleNamespace(
        returncode=returncode, stdout=stdout, stderr=stderr,
    )


def test_probe_reports_size_and_mtime_on_success(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Happy path: remote PowerShell says OK with size + mtime. The
    # probe returns exists=True with the parsed size and the full
    # detail line preserved for forensic logging.
    stdout = (
        "OK size=452837 mtime=2026-04-24T18:12:03.1234567Z "
        "path=C:\\Users\\ci\\shipyard.bundle"
    )
    with patch(
        "shipyard.executor.ssh_windows.subprocess.run",
        return_value=_proc(stdout=stdout),
    ):
        result = _probe_remote_bundle(
            host="win", bundle_path="shipyard.bundle",
            ssh_options=["-o", "ConnectTimeout=5"],
        )
    assert isinstance(result, _BundleProbe)
    assert result.exists is True
    assert result.size == 452837
    assert "size=452837" in result.detail
    assert "mtime=" in result.detail


def test_probe_detects_missing_file(monkeypatch: pytest.MonkeyPatch) -> None:
    # The exact #247 failure mode: upload_bundle claimed success,
    # but PowerShell's Test-Path says the file is not there.
    stdout = "MISSING path=C:\\Users\\ci\\shipyard.bundle"
    with patch(
        "shipyard.executor.ssh_windows.subprocess.run",
        return_value=_proc(stdout=stdout),
    ):
        result = _probe_remote_bundle(
            host="win", bundle_path="shipyard.bundle", ssh_options=[],
        )
    assert result.exists is False
    assert result.size == 0
    assert "MISSING" in result.detail


def test_probe_handles_subprocess_timeout(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Timeouts during the probe itself are treated as "can't verify".
    # The caller bails rather than handing off to git on a state it
    # couldn't confirm — otherwise we just reintroduce the #247
    # CLIXML-wrapped garbage on a slower / less-responsive host.
    def raise_timeout(*_args, **_kw):
        raise subprocess.TimeoutExpired(cmd=["ssh"], timeout=30)

    with patch(
        "shipyard.executor.ssh_windows.subprocess.run",
        side_effect=raise_timeout,
    ):
        result = _probe_remote_bundle(
            host="win", bundle_path="shipyard.bundle", ssh_options=[],
        )
    assert result.exists is False
    assert result.size == 0
    assert "timed out" in result.detail.lower()


def test_probe_handles_subprocess_error(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # OSError (e.g. ssh binary gone) returns exists=False with a
    # diagnostic so validate() can surface a clean error message.
    def raise_os_error(*_args, **_kw):
        raise OSError("ssh: command not found")

    with patch(
        "shipyard.executor.ssh_windows.subprocess.run",
        side_effect=raise_os_error,
    ):
        result = _probe_remote_bundle(
            host="win", bundle_path="shipyard.bundle", ssh_options=[],
        )
    assert result.exists is False
    assert "probe error" in result.detail


def test_probe_nonzero_exit_preserves_stderr_snippet(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # PowerShell exited non-zero (e.g. auth broke, PowerShell
    # unavailable on the remote). The probe surfaces exit code +
    # stderr snippet so the operator has something to grep.
    with patch(
        "shipyard.executor.ssh_windows.subprocess.run",
        return_value=_proc(
            returncode=255,
            stderr="Permission denied (publickey).",
        ),
    ):
        result = _probe_remote_bundle(
            host="win", bundle_path="shipyard.bundle", ssh_options=[],
        )
    assert result.exists is False
    assert "exited 255" in result.detail
    assert "Permission denied" in result.detail


def test_probe_treats_unparseable_stdout_as_failure(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Neither the OK sentinel nor MISSING is present (garbled
    # output, e.g. PowerShell crashed mid-pipe). The probe must
    # NOT default to exists=True — that'd reintroduce the #247
    # failure mode where a missing-file state sneaks past.
    with patch(
        "shipyard.executor.ssh_windows.subprocess.run",
        return_value=_proc(stdout="welcome banner\nsomething else"),
    ):
        result = _probe_remote_bundle(
            host="win", bundle_path="shipyard.bundle", ssh_options=[],
        )
    assert result.exists is False
    assert "unexpected output" in result.detail


def test_probe_tolerates_powershell_profile_banner(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Real-world: operator's PowerShell profile prints a banner on
    # session open. Our OK line ends up on line 2+, not line 1.
    # Requiring our sentinel at stdout position 0 would false-
    # positive fail on those hosts, so we scan anywhere in stdout
    # for the sentinel. This test pins the tolerance.
    stdout = (
        "Welcome to PowerShell 7.4, Administrator.\n"
        "Loading user profile from $PROFILE...\n"
        "OK size=12345 mtime=2026-04-24T18:12:03.0Z "
        "path=C:\\Users\\ci\\shipyard.bundle"
    )
    with patch(
        "shipyard.executor.ssh_windows.subprocess.run",
        return_value=_proc(stdout=stdout),
    ):
        result = _probe_remote_bundle(
            host="win", bundle_path="shipyard.bundle", ssh_options=[],
        )
    assert result.exists is True
    assert result.size == 12345

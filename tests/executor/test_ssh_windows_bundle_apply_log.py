"""Bundle-apply failure persists raw PowerShell stderr to disk (#200).

v0.34.0 (#189) wired a CLIXML decoder into the bundle-apply error
path, but the decoder receives only whatever subprocess stderr
contains. Real-world pulp failures today surfaced ``#< CLIXML`` with
no body — the decoder has nothing to parse — and ``windows.log``
doesn't exist yet at bundle-apply time, so there's zero forensic
record.

This test pins the post-fix contract: when ``_apply_bundle_windows``
fails, the raw stderr is written to ``<log_file>.bundle-apply-stderr``
next to the intended log file, regardless of whether the CLIXML
decoder succeeds.
"""

from __future__ import annotations

from pathlib import Path
from unittest.mock import patch

import pytest  # noqa: TC002 — used at runtime via MonkeyPatch fixture

from shipyard.executor.ssh_windows import _apply_bundle_windows


class _FakeCompletedProcess:
    def __init__(self, returncode: int, stderr: str = "", stdout: str = "") -> None:
        self.returncode = returncode
        self.stderr = stderr
        self.stdout = stdout


def test_bundle_apply_failure_writes_raw_stderr_log(tmp_path: Path) -> None:
    log_file = tmp_path / "run-id" / "windows.log"
    fake_stderr = "#< CLIXML\n<Objs><S S=\"Error\">bundle verify: not a valid bundle</S></Objs>"

    with patch(
        "shipyard.executor.ssh_windows.subprocess.run",
        return_value=_FakeCompletedProcess(returncode=1, stderr=fake_stderr),
    ):
        result = _apply_bundle_windows(
            host="win",
            bundle_path="shipyard.bundle",
            repo_path="~/repo",
            ssh_options=[],
            log_file=log_file,
        )

    assert result.success is False
    # Raw stderr log lands right next to where windows.log would be.
    sibling = Path(str(log_file) + ".bundle-apply-stderr")
    assert sibling.exists(), "bundle-apply stderr log must be written on failure"
    body = sibling.read_text()
    assert "exit_code=1" in body
    assert "#< CLIXML" in body
    assert "bundle verify: not a valid bundle" in body
    # And the ApplyResult message references the log path so the
    # user can find it from the summary row.
    assert str(sibling) in result.message


def test_bundle_apply_failure_with_empty_envelope_still_logs_raw(
    tmp_path: Path,
) -> None:
    """#200 specifically: a truncated envelope (sentinel-only, no
    body) left the user with ``#< CLIXML`` and no log artifact at
    all. The log write must still fire so next time we have bytes
    to analyze."""
    log_file = tmp_path / "run-id" / "windows.log"
    fake_stderr = "#< CLIXML\n"  # sentinel only, no <Objs> body

    with patch(
        "shipyard.executor.ssh_windows.subprocess.run",
        return_value=_FakeCompletedProcess(returncode=1, stderr=fake_stderr),
    ):
        result = _apply_bundle_windows(
            host="win",
            bundle_path="shipyard.bundle",
            repo_path="~/repo",
            ssh_options=[],
            log_file=log_file,
        )

    sibling = Path(str(log_file) + ".bundle-apply-stderr")
    assert sibling.exists()
    assert "#< CLIXML" in sibling.read_text()
    assert result.success is False


def test_bundle_apply_success_does_not_write_log(tmp_path: Path) -> None:
    log_file = tmp_path / "run-id" / "windows.log"

    with patch(
        "shipyard.executor.ssh_windows.subprocess.run",
        return_value=_FakeCompletedProcess(returncode=0, stderr="", stdout="ok"),
    ):
        result = _apply_bundle_windows(
            host="win",
            bundle_path="shipyard.bundle",
            repo_path="~/repo",
            ssh_options=[],
            log_file=log_file,
        )

    assert result.success is True
    sibling = Path(str(log_file) + ".bundle-apply-stderr")
    assert not sibling.exists(), "success path must not write a failure log"


def test_bundle_apply_failure_without_log_file_still_returns_error(
    tmp_path: Path,
) -> None:
    """Callers may pass log_file=None (e.g. in unit tests); the
    error path must still return a clean ApplyResult, just without
    the log reference."""
    with patch(
        "shipyard.executor.ssh_windows.subprocess.run",
        return_value=_FakeCompletedProcess(returncode=1, stderr="boom"),
    ):
        result = _apply_bundle_windows(
            host="win",
            bundle_path="shipyard.bundle",
            repo_path="~/repo",
            ssh_options=[],
            log_file=None,
        )
    assert result.success is False
    assert "Remote bundle apply failed" in result.message
    assert "raw stderr:" not in result.message


def test_bundle_apply_log_write_failure_does_not_mask_original_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Best-effort logging: if the log write itself fails, the
    primary ApplyResult must still carry the decoded message and
    success=False. We simulate a filesystem-write OSError."""
    # Point at a path whose parent creation will fail — null-byte
    # in name triggers OSError on mkdir/write without any real IO.
    log_file = tmp_path / "run-id" / "windows.log"

    original_write_text = Path.write_text

    def _raise(self: Path, *args, **kwargs) -> int:
        if str(self).endswith(".bundle-apply-stderr"):
            raise OSError("simulated: read-only filesystem")
        return original_write_text(self, *args, **kwargs)

    monkeypatch.setattr(Path, "write_text", _raise)

    with patch(
        "shipyard.executor.ssh_windows.subprocess.run",
        return_value=_FakeCompletedProcess(returncode=1, stderr="some stderr"),
    ):
        result = _apply_bundle_windows(
            host="win",
            bundle_path="shipyard.bundle",
            repo_path="~/repo",
            ssh_options=[],
            log_file=log_file,
        )

    assert result.success is False
    assert "Remote bundle apply failed" in result.message
    # Log reference must NOT appear since the log write failed.
    assert "raw stderr:" not in result.message

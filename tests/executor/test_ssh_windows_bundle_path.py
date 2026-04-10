"""Regression tests for Windows bundle path defaulting.

Stage 1 dogfooding surfaced a Windows scp failure because the
default `remote_bundle_path` was `C:\\Temp\\shipyard.bundle` and
`C:\\Temp` doesn't exist on a stock Windows install. Fix: the
default is now a bare filename, which lands in the SSH user's
home directory; the PowerShell apply command expands it via
`Join-Path $HOME <path>` when it isn't absolute.
"""

from __future__ import annotations

from shipyard.executor.ssh_windows import _is_windows_absolute_path

# ── _is_windows_absolute_path ──────────────────────────────────────────


def test_drive_letter_path_is_absolute() -> None:
    assert _is_windows_absolute_path("C:\\Users\\me\\foo.bundle") is True
    assert _is_windows_absolute_path("D:/repo/bundle") is True


def test_unc_path_is_absolute() -> None:
    assert _is_windows_absolute_path("\\\\server\\share\\file") is True


def test_leading_separator_is_absolute() -> None:
    assert _is_windows_absolute_path("\\foo\\bar") is True
    assert _is_windows_absolute_path("/foo/bar") is True


def test_bare_filename_is_not_absolute() -> None:
    assert _is_windows_absolute_path("shipyard.bundle") is False


def test_relative_path_is_not_absolute() -> None:
    assert _is_windows_absolute_path("Temp\\shipyard.bundle") is False


def test_empty_path_is_not_absolute() -> None:
    assert _is_windows_absolute_path("") is False


# ── apply command shape ────────────────────────────────────────────────


def test_apply_command_uses_home_for_relative_path() -> None:
    """A relative bundle path must be resolved via Join-Path $HOME."""
    from unittest.mock import patch

    from shipyard.executor.ssh_windows import (
        _apply_bundle_windows,
        decode_encoded_ssh_argv,
    )

    captured: list[list[str]] = []

    def fake_run(cmd, *args, **kwargs):
        captured.append(list(cmd))
        import subprocess
        return subprocess.CompletedProcess(
            args=cmd, returncode=0, stdout="", stderr="",
        )

    with patch("subprocess.run", side_effect=fake_run):
        _apply_bundle_windows(
            host="win",
            bundle_path="shipyard.bundle",
            repo_path="C:\\repo",
            ssh_options=[],
        )

    assert len(captured) == 1
    # Bundle apply now also goes through -EncodedCommand for the
    # same multi-line-drop reason the main validate path does.
    assert "-EncodedCommand" in captured[0]
    ps_cmd = decode_encoded_ssh_argv(captured[0])
    assert ps_cmd is not None
    assert "Join-Path $HOME 'shipyard.bundle'" in ps_cmd
    assert "$Bundle = " in ps_cmd


def test_apply_command_uses_literal_absolute_path() -> None:
    """An absolute bundle path is used verbatim, not wrapped in Join-Path."""
    from unittest.mock import patch

    from shipyard.executor.ssh_windows import (
        _apply_bundle_windows,
        decode_encoded_ssh_argv,
    )

    captured: list[list[str]] = []

    def fake_run(cmd, *args, **kwargs):
        captured.append(list(cmd))
        import subprocess
        return subprocess.CompletedProcess(
            args=cmd, returncode=0, stdout="", stderr="",
        )

    with patch("subprocess.run", side_effect=fake_run):
        _apply_bundle_windows(
            host="win",
            bundle_path="C:\\Users\\me\\my.bundle",
            repo_path="C:\\repo",
            ssh_options=[],
        )

    ps_cmd = decode_encoded_ssh_argv(captured[0])
    assert ps_cmd is not None
    assert "'C:\\Users\\me\\my.bundle'" in ps_cmd
    assert "Join-Path" not in ps_cmd

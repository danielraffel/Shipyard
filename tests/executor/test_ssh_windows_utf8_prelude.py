"""Regression tests for the UTF-8 code-page prelude in ssh-windows
PowerShell commands (#208).

Pulp hit 46 spurious ctest failures on Namespace Windows because the
host defaulted to CP-1252 and em-dash test names got mangled through
the argv round-trip. Shipyard owns the command wrapping, so the fix
belongs here — prepend a three-line UTF-8 setter to every PS command
we dispatch.
"""

from __future__ import annotations

from shipyard.executor.ssh_windows import (
    _WINDOWS_UTF8_PRELUDE,
    _build_remote_command,
)


def test_prelude_contains_all_three_encoding_settings() -> None:
    # Three settings cover three different encoding paths. Any one
    # missing leaves a window for mojibake — assert all three by
    # exact substring.
    assert "chcp.com 65001" in _WINDOWS_UTF8_PRELUDE
    assert "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8" in _WINDOWS_UTF8_PRELUDE
    assert "$OutputEncoding = [System.Text.Encoding]::UTF8" in _WINDOWS_UTF8_PRELUDE


def test_prelude_runs_before_user_commands_in_build_remote_command() -> None:
    cmd = _build_remote_command(
        sha="abc123",
        remote_repo="C:/repo",
        validation_config={"command": "ctest --output-on-failure"},
    )
    assert cmd is not None
    # Prelude must be the FIRST thing in the command — any user code
    # that runs before the prelude would still see CP-1252.
    assert cmd.startswith(_WINDOWS_UTF8_PRELUDE), (
        f"prelude must prefix the PS command; got: {cmd[:200]!r}"
    )
    # And the user command itself must still land intact downstream.
    assert "ctest --output-on-failure" in cmd
    assert "git checkout --force 'abc123'" in cmd


def test_prelude_prepended_to_staged_validation() -> None:
    # The multi-stage path (build / test / lint) also goes through
    # _build_remote_command; make sure the prelude isn't bypassed
    # when the user supplies stages instead of a single command.
    cmd = _build_remote_command(
        sha="feedface",
        remote_repo="C:/repo",
        validation_config={
            "build": "cmake --build build",
            "test": "ctest --output-on-failure",
        },
    )
    assert cmd is not None
    assert cmd.startswith(_WINDOWS_UTF8_PRELUDE)
    assert "cmake --build build" in cmd
    assert "ctest --output-on-failure" in cmd


def test_prelude_order_matters() -> None:
    # chcp.com AFTER the PowerShell encoding setters ensures the
    # console CP change doesn't reset what PowerShell is already
    # using for its I/O. Assert the order so a future refactor
    # doesn't silently shuffle it.
    prelude = _WINDOWS_UTF8_PRELUDE
    console_idx = prelude.index("[Console]::OutputEncoding")
    output_idx = prelude.index("$OutputEncoding")
    chcp_idx = prelude.index("chcp.com")
    assert console_idx < chcp_idx, "console encoding must be set before chcp"
    assert output_idx < chcp_idx, "$OutputEncoding must be set before chcp"


def test_prelude_suppresses_chcp_confirmation_output() -> None:
    # `chcp.com 65001` normally prints "Active code page: 65001"
    # which would pollute the validation log. `| Out-Null`
    # suppresses it. Without this the log would carry one extra
    # decoration line per run.
    assert "| Out-Null" in _WINDOWS_UTF8_PRELUDE

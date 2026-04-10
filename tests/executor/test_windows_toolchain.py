"""Unit tests for the Windows toolchain helpers.

These tests don't shell out to a real Windows host — they exercise
the pure PowerShell-builder functions and the JSON-parsing branch of
`detect_vs_toolchain` via mocks.
"""

from __future__ import annotations

import subprocess
from unittest.mock import patch

from shipyard.executor.windows_toolchain import (
    DEFAULT_MUTEX_NAME,
    VsToolchain,
    detect_vs_toolchain,
    toolchain_env_exports,
    wrap_powershell_with_host_mutex,
)

# ── wrap_powershell_with_host_mutex ─────────────────────────────────────


def test_mutex_wrapper_includes_default_name() -> None:
    wrapped = wrap_powershell_with_host_mutex("echo hello")
    assert DEFAULT_MUTEX_NAME in wrapped
    # Global\\ prefix is preserved (with backslash intact inside
    # the emitted PS single-quoted literal).
    assert "Global\\ShipyardValidate" in wrapped


def test_mutex_wrapper_accepts_custom_name() -> None:
    wrapped = wrap_powershell_with_host_mutex(
        "echo hello",
        mutex_name="Global\\MyProjectCI",
    )
    assert "Global\\MyProjectCI" in wrapped


def test_mutex_wrapper_escapes_single_quotes_in_name() -> None:
    """A mutex name containing ' must not break out of its literal."""
    wrapped = wrap_powershell_with_host_mutex(
        "echo hello",
        mutex_name="Local\\it's-fine",
    )
    # PowerShell escapes single quotes by doubling them inside a
    # single-quoted string. The original ' should never appear
    # unescaped in the emitted literal.
    assert "'Local\\it''s-fine'" in wrapped


def test_mutex_wrapper_preserves_body_and_exits_with_its_code() -> None:
    wrapped = wrap_powershell_with_host_mutex("cmake --build build")
    assert "cmake --build build" in wrapped
    # The wrapper captures $LASTEXITCODE and re-exits with it.
    assert "$__ShipyardExit = $LASTEXITCODE" in wrapped
    assert "exit $__ShipyardExit" in wrapped


def test_mutex_wrapper_emits_wait_markers() -> None:
    wrapped = wrap_powershell_with_host_mutex("echo hi")
    # These two markers let log readers distinguish "blocked on the
    # host lock" from "stuck somewhere during build".
    assert "__SHIPYARD_WAIT__:host-lock" in wrapped
    assert "__SHIPYARD_PHASE__:waiting-lock" in wrapped


def test_mutex_wrapper_handles_abandoned_mutex() -> None:
    """A crashed prior run leaves an abandoned mutex; we must recover."""
    wrapped = wrap_powershell_with_host_mutex("echo hi")
    assert "AbandonedMutexException" in wrapped
    assert "Recovered abandoned host validation lock" in wrapped


def test_mutex_wrapper_releases_and_disposes() -> None:
    wrapped = wrap_powershell_with_host_mutex("echo hi")
    assert "$Mutex.ReleaseMutex()" in wrapped
    assert "$Mutex.Dispose()" in wrapped


# ── toolchain_env_exports ───────────────────────────────────────────────


def test_env_exports_none_toolchain_sets_empty_vars() -> None:
    """A None toolchain exports empty strings so stages can still guard on them."""
    snippet = toolchain_env_exports(None)
    assert "$env:SHIPYARD_CMAKE_PLATFORM = ''" in snippet
    assert "$env:SHIPYARD_CMAKE_GENERATOR_INSTANCE = ''" in snippet


def test_env_exports_arm64_toolchain() -> None:
    toolchain = VsToolchain(
        cmake_platform="ARM64",
        cmake_generator_instance="C:/Program Files/Microsoft Visual Studio/2022/Community",
    )
    snippet = toolchain_env_exports(toolchain)
    assert "$env:SHIPYARD_CMAKE_PLATFORM = 'ARM64'" in snippet
    assert "Community" in snippet


def test_env_exports_escapes_single_quotes_in_path() -> None:
    """An install path with a single quote must not break the literal."""
    toolchain = VsToolchain(
        cmake_platform="x64",
        cmake_generator_instance="C:/it's/weird/vs",
    )
    snippet = toolchain_env_exports(toolchain)
    assert "'C:/it''s/weird/vs'" in snippet


# ── vswhere script sanity ──────────────────────────────────────────────


def test_vswhere_script_does_not_pass_latest_flag() -> None:
    """Regression for Codex finding on PR #5.

    `-latest` returns only one install, which would defeat the
    "prefer non-BuildTools" Where-Object filter when Build Tools is
    the most-recently installed product. The script must enumerate
    every install and let the filter pick.

    Check the actual vswhere invocation line, not the surrounding
    comment which is allowed to mention the flag for context.
    """
    from shipyard.executor.windows_toolchain import _VS_DETECT_SCRIPT
    # Extract the actual vswhere invocation line
    vswhere_lines = [
        line for line in _VS_DETECT_SCRIPT.splitlines()
        if "& $vswhere" in line
    ]
    assert len(vswhere_lines) == 1, f"expected one vswhere call, got: {vswhere_lines}"
    assert "-latest" not in vswhere_lines[0]
    assert "-products" in vswhere_lines[0]
    assert "-format json" in vswhere_lines[0]
    assert "vswhere.exe" in _VS_DETECT_SCRIPT
    assert "Microsoft.VisualStudio.Product.BuildTools" in _VS_DETECT_SCRIPT


# ── detect_vs_toolchain ─────────────────────────────────────────────────


def _mock_run(returncode: int = 0, stdout: str = "", stderr: str = ""):
    return subprocess.CompletedProcess(
        args=[], returncode=returncode, stdout=stdout, stderr=stderr,
    )


def test_detect_returns_none_when_ssh_fails() -> None:
    with patch("subprocess.run", side_effect=OSError("ssh boom")):
        assert detect_vs_toolchain("host", []) is None


def test_detect_returns_none_when_powershell_exits_nonzero() -> None:
    with patch("subprocess.run", return_value=_mock_run(returncode=1)):
        assert detect_vs_toolchain("host", []) is None


def test_detect_returns_none_when_timeout() -> None:
    with patch(
        "subprocess.run",
        side_effect=subprocess.TimeoutExpired(cmd="ssh", timeout=60),
    ):
        assert detect_vs_toolchain("host", []) is None


def test_detect_parses_valid_json_output() -> None:
    stdout = (
        "some preamble\n"
        '{"platform":"ARM64","generator_instance":"C:/VS/2022/Community"}\n'
    )
    with patch("subprocess.run", return_value=_mock_run(stdout=stdout)):
        toolchain = detect_vs_toolchain("host", [])
    assert toolchain is not None
    assert toolchain.cmake_platform == "ARM64"
    assert toolchain.cmake_generator_instance == "C:/VS/2022/Community"


def test_detect_ignores_banner_lines() -> None:
    """Some PowerShell profiles print a banner — we scan from the bottom."""
    stdout = (
        "Windows PowerShell\nCopyright (C) Microsoft\n"
        '{"platform":"x64","generator_instance":""}\n'
    )
    with patch("subprocess.run", return_value=_mock_run(stdout=stdout)):
        toolchain = detect_vs_toolchain("host", [])
    assert toolchain is not None
    assert toolchain.cmake_platform == "x64"
    assert toolchain.cmake_generator_instance == ""


def test_detect_returns_none_on_malformed_json() -> None:
    stdout = "this is not json at all\n"
    with patch("subprocess.run", return_value=_mock_run(stdout=stdout)):
        assert detect_vs_toolchain("host", []) is None


def test_detect_returns_none_when_both_fields_empty() -> None:
    """A vswhere-less host returns both fields empty → not useful, return None."""
    stdout = '{"platform":"","generator_instance":""}\n'
    with patch("subprocess.run", return_value=_mock_run(stdout=stdout)):
        assert detect_vs_toolchain("host", []) is None


def test_detect_uses_encoded_command_not_stdin() -> None:
    """The detect script must travel via -EncodedCommand, not -Command -.

    Regression test: an earlier version used `powershell -Command -` +
    stdin, which silently failed on Windows because PowerShell parses
    stdin line-by-line and the detect script has multi-line `function`
    and `try { ... } catch { ... }` blocks. The result was rc=0 with
    empty stdout — silently masked by `detect_vs_toolchain` falling
    back to None and the executor using CMake defaults. The bug was
    invisible until the broader Windows false-green investigation
    surfaced it.
    """
    captured: list[list[str]] = []

    def fake_run(cmd, *args, **kwargs):
        captured.append(list(cmd))
        # `input=` must NOT be passed — the script travels in argv.
        assert "input" not in kwargs, (
            "detect_vs_toolchain must not pipe the script via stdin; "
            "PowerShell -Command - parses stdin line-by-line and "
            "silently drops multi-line constructs."
        )
        return _mock_run(stdout='{"platform":"x64","generator_instance":"C:/VS"}\n')

    with patch("subprocess.run", side_effect=fake_run):
        result = detect_vs_toolchain("host", [])

    assert len(captured) == 1
    assert "-EncodedCommand" in captured[0]
    assert "-Command" not in captured[0]
    assert result is not None

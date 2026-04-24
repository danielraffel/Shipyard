"""Tests for doctor's macOS Gatekeeper / quarantine / codesign smoke (#216).

The #216 symptom — "exit 137 once, then exit 0 with zero output
forever" — is macOS caching a Gatekeeper rejection of a binary
whose Developer-ID signature got destroyed (pre-v0.36.0 install.sh
ad-hoc resign). This check catches the broken state in doctor so
the operator sees "spctl rejection: …" instead of silence.

The check is macOS-only and only meaningful against a frozen
PyInstaller bundle, so we mock sys.platform + sys.frozen to
exercise the Windows/Linux and dev-env paths deterministically.
"""

from __future__ import annotations

import sys
from types import SimpleNamespace
from typing import Any
from unittest.mock import patch

import pytest  # noqa: TC002 — used at runtime via MonkeyPatch fixture

from shipyard.cli import _check_macos_gatekeeper_health


def _ok_proc(returncode: int = 0, stdout: str = "", stderr: str = "") -> Any:
    return SimpleNamespace(returncode=returncode, stdout=stdout, stderr=stderr)


def test_skips_on_non_macos(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(sys, "platform", "linux")
    assert _check_macos_gatekeeper_health() is None


def test_skips_on_non_frozen_darwin(monkeypatch: pytest.MonkeyPatch) -> None:
    # Running from source: Python itself is always signed OK, so a
    # passing row here would be false confidence about the release
    # binary. Skip.
    monkeypatch.setattr(sys, "platform", "darwin")
    monkeypatch.setattr(sys, "frozen", False, raising=False)
    assert _check_macos_gatekeeper_health() is None


def _force_frozen_macos(monkeypatch: pytest.MonkeyPatch, tmp_path) -> None:
    """Pretend we're a frozen PyInstaller bundle on macOS.

    `sys.executable` gets pointed at a real file under tmp_path so
    the existence check passes; we don't care about its contents.
    """
    binary = tmp_path / "shipyard"
    binary.write_bytes(b"stub")
    monkeypatch.setattr(sys, "platform", "darwin")
    monkeypatch.setattr(sys, "frozen", True, raising=False)
    monkeypatch.setattr(sys, "executable", str(binary))


def test_all_green_returns_ok(
    monkeypatch: pytest.MonkeyPatch, tmp_path
) -> None:
    _force_frozen_macos(monkeypatch, tmp_path)

    def fake_run(cmd, **kw):
        # xattr: no quarantine. spctl + codesign: success.
        return _ok_proc(returncode=0, stdout="", stderr="")

    with patch("shipyard.cli.subprocess.run", side_effect=fake_run):
        result = _check_macos_gatekeeper_health()

    assert result is not None
    assert result["ok"] is True


def test_quarantine_xattr_flagged(
    monkeypatch: pytest.MonkeyPatch, tmp_path
) -> None:
    _force_frozen_macos(monkeypatch, tmp_path)

    calls = {"n": 0}

    def fake_run(cmd, **kw):
        calls["n"] += 1
        # First call is xattr listing — include the quarantine flag.
        if cmd[0] == "xattr":
            return _ok_proc(stdout="com.apple.quarantine\n")
        # spctl + codesign accept the binary.
        return _ok_proc(returncode=0)

    with patch("shipyard.cli.subprocess.run", side_effect=fake_run):
        result = _check_macos_gatekeeper_health()

    assert result is not None
    assert result["ok"] is False
    assert "quarantine" in result["detail"].lower()
    # Fix instruction must name the actual xattr -d invocation.
    assert "xattr -d com.apple.quarantine" in result["detail"]


def test_spctl_rejection_is_NOT_flagged_anymore(  # noqa: N802 — load-bearing test name
    monkeypatch: pytest.MonkeyPatch, tmp_path
) -> None:
    # #231: spctl --assess rejects v0.44.0 stapled-dmg-extracted CLI
    # binaries by default policy even though the binary launches
    # cleanly under taskgated. We intentionally DO NOT probe spctl
    # anymore — a rejection there doesn't predict real-world
    # launch failure. This test locks that decision in: if spctl
    # is invoked despite the rejection policy, the test fails
    # (spctl being called at all means someone reintroduced the
    # probe).
    _force_frozen_macos(monkeypatch, tmp_path)
    spctl_called = {"n": 0}

    def fake_run(cmd, **kw):
        if cmd[0] == "spctl":
            spctl_called["n"] += 1
        return _ok_proc(returncode=0)

    with patch("shipyard.cli.subprocess.run", side_effect=fake_run):
        result = _check_macos_gatekeeper_health()

    assert result is not None
    assert result["ok"] is True, (
        "Binary with healthy xattr + codesign must be marked ok "
        "regardless of spctl policy"
    )
    assert spctl_called["n"] == 0, (
        "spctl probe was removed per #231 — reintroducing it would "
        "false-positive on v0.44.0+ stapled-dmg installs"
    )


def test_codesign_verify_failure_flagged(
    monkeypatch: pytest.MonkeyPatch, tmp_path
) -> None:
    _force_frozen_macos(monkeypatch, tmp_path)

    def fake_run(cmd, **kw):
        if cmd[0] == "codesign":
            return _ok_proc(returncode=1, stderr="code object is not signed at all")
        return _ok_proc(returncode=0)

    with patch("shipyard.cli.subprocess.run", side_effect=fake_run):
        result = _check_macos_gatekeeper_health()

    assert result is not None
    assert result["ok"] is False
    assert "codesign" in result["detail"].lower()


def test_multiple_real_problems_all_surface(
    monkeypatch: pytest.MonkeyPatch, tmp_path
) -> None:
    # Compounded case: quarantine xattr AND broken codesign. Both
    # must appear in the detail so the operator has the full
    # picture. Previously this test also checked spctl rejection,
    # but #231 removed that probe because it false-positives on
    # stapled-dmg-extracted CLI binaries. Now we check the two
    # REAL problems side-by-side.
    _force_frozen_macos(monkeypatch, tmp_path)

    def fake_run(cmd, **kw):
        if cmd[0] == "xattr":
            return _ok_proc(stdout="com.apple.quarantine\n")
        if cmd[0] == "codesign":
            return _ok_proc(returncode=1, stderr="code object is not signed at all")
        return _ok_proc(returncode=0)

    with patch("shipyard.cli.subprocess.run", side_effect=fake_run):
        result = _check_macos_gatekeeper_health()

    assert result is not None
    assert result["ok"] is False
    assert "quarantine" in result["detail"].lower()
    assert "codesign" in result["detail"].lower()


def test_missing_tool_falls_through_without_raising(
    monkeypatch: pytest.MonkeyPatch, tmp_path
) -> None:
    # A machine without xattr / spctl / codesign on PATH should not
    # blow up doctor — those probes are individually try/excepted and
    # either pass (no detected problems) or contribute nothing.
    _force_frozen_macos(monkeypatch, tmp_path)

    def fake_run(cmd, **kw):
        raise FileNotFoundError(cmd[0])

    with patch("shipyard.cli.subprocess.run", side_effect=fake_run):
        result = _check_macos_gatekeeper_health()

    # All three probes silently skipped → no problems detected → ok.
    assert result is not None
    assert result["ok"] is True

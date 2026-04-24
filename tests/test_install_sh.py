"""Tests for the environment-variable contract of install.sh.

install.sh's default install location and version resolution are what
downstream consumers (Claude Code plugin's auto-installer, Codex one-
liner, project pinners like pulp) depend on. Regressions here either
fragment the install footprint (multiple shipyard binaries in
different places) or break version-pinned installers.

We drive install.sh with ``SHIPYARD_DRY_RUN=1`` which skips the
network + filesystem work and prints the resolved config as
KEY=value pairs. Platform detection (OS=macos/linux/windows) is
whatever host runs the test; we only assert invariants that hold on
every platform.
"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import pytest

# install.sh is POSIX shell and the tests drive it via `bash`. On
# Windows, Git-for-Windows bash exits non-zero on the very first
# `uname -m` resolution, and Windows doesn't populate `$HOME` so
# assertions that derive the expected path from `os.environ["HOME"]`
# throw KeyError. The installer itself isn't shipped for Windows
# users — they use the winget/msi path (when that exists) or the
# plugin's bundled binary. Linux + macOS coverage here is enough.
pytestmark = pytest.mark.skipif(
    sys.platform == "win32",
    reason="install.sh is a POSIX shell script; Linux+macOS runners provide full coverage",
)

REPO_ROOT = Path(__file__).resolve().parents[1]
INSTALL_SH = REPO_ROOT / "install.sh"


def _run_dry(env: dict[str, str] | None = None) -> dict[str, str]:
    """Run install.sh in dry-run mode; parse KEY=value output."""
    merged_env = {**os.environ, "SHIPYARD_DRY_RUN": "1"}
    if env:
        merged_env.update(env)
    result = subprocess.run(
        ["bash", str(INSTALL_SH)],
        env=merged_env,
        capture_output=True,
        text=True,
        check=True,
    )
    out: dict[str, str] = {}
    for line in result.stdout.splitlines():
        if "=" in line:
            key, _, value = line.partition("=")
            out[key.strip()] = value.strip()
    return out


def test_default_install_dir_is_local_bin() -> None:
    # The canonical install location is `~/.local/bin`. Downstream
    # consumers (plugin's check-cli.sh, Codex one-liner, any wrapper)
    # rely on this. Changing the default is a compatibility break.
    home = os.environ["HOME"]
    config = _run_dry()
    assert config["INSTALL_DIR"] == f"{home}/.local/bin"


def test_shipyard_install_dir_env_overrides(tmp_path: Path) -> None:
    config = _run_dry({"SHIPYARD_INSTALL_DIR": str(tmp_path / "bin")})
    assert config["INSTALL_DIR"] == str(tmp_path / "bin")


def test_default_version_resolves_to_latest() -> None:
    config = _run_dry()
    assert config["VERSION_LABEL"] == "latest"
    assert config["API_PATH"] == "releases/latest"


def test_explicit_latest_matches_default() -> None:
    config = _run_dry({"SHIPYARD_VERSION": "latest"})
    assert config["API_PATH"] == "releases/latest"


@pytest.mark.parametrize(
    "raw,expected_label,expected_api",
    [
        ("v0.22.1", "v0.22.1", "releases/tags/v0.22.1"),
        ("0.22.1", "v0.22.1", "releases/tags/v0.22.1"),  # shorthand normalization
        ("v1.0.0-rc.1", "v1.0.0-rc.1", "releases/tags/v1.0.0-rc.1"),
    ],
)
def test_shipyard_version_pins_specific_tag(
    raw: str, expected_label: str, expected_api: str
) -> None:
    config = _run_dry({"SHIPYARD_VERSION": raw})
    assert config["VERSION_LABEL"] == expected_label
    assert config["API_PATH"] == expected_api


def test_empty_shipyard_version_falls_back_to_latest() -> None:
    config = _run_dry({"SHIPYARD_VERSION": ""})
    assert config["API_PATH"] == "releases/latest"


def test_artifact_matches_platform() -> None:
    # ARTIFACT should always start with "shipyard-" and combine the
    # detected OS + ARCH. Exact values depend on the test host.
    config = _run_dry()
    assert config["ARTIFACT"].startswith("shipyard-")
    assert config["OS"] in ("macos", "linux", "windows")
    assert config["ARCH"] in ("arm64", "x64")
    assert config["ARTIFACT"] == f"shipyard-{config['OS']}-{config['ARCH']}"


def test_install_dir_override_does_not_affect_version_resolution() -> None:
    # Sanity: env vars are independent.
    config = _run_dry(
        {
            "SHIPYARD_INSTALL_DIR": "/tmp/foo",
            "SHIPYARD_VERSION": "v0.22.1",
        }
    )
    assert config["INSTALL_DIR"] == "/tmp/foo"
    assert config["API_PATH"] == "releases/tags/v0.22.1"


# -- #219: post-install smoke + remediation -------------------------
# install.sh now runs the freshly-installed binary's `--version` and
# fails loud (exit 1, specific error messages) if it can't launch.
# This is the first line of defense against the v0.42.0 taskgated
# SIGKILL class of bug where `codesign --verify` passes but the
# binary dies at runtime. Testability hook: SHIPYARD_SKIP_DOWNLOAD=1
# reuses an existing binary at $INSTALL_DIR/shipyard so we can
# inject a stub that succeeds or fails deterministically.

def _install_with_stub(
    tmp_path: Path,
    *,
    stub_behaviour: str,
    extra_env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    """Drive install.sh against a tmp install dir with a stub binary
    pre-planted at ``$INSTALL_DIR/shipyard``.

    ``stub_behaviour`` is either ``"ok"`` (script exits 0 with a
    version line) or ``"sigkill"`` (script exits 137 with no output,
    simulating taskgated rejection).
    """
    install_dir = tmp_path / "bin"
    install_dir.mkdir()
    stub = install_dir / "shipyard"
    if stub_behaviour == "ok":
        stub.write_text("#!/bin/sh\necho shipyard 99.99.99\n")
    elif stub_behaviour == "sigkill":
        # kill -KILL $$ is the closest deterministic proxy for the
        # real taskgated SIGKILL: no stdout, no stderr, exit 137.
        stub.write_text("#!/bin/sh\nkill -KILL $$\n")
    else:
        raise ValueError(stub_behaviour)
    stub.chmod(0o755)

    env = {
        **os.environ,
        "SHIPYARD_INSTALL_DIR": str(install_dir),
        "SHIPYARD_SKIP_DOWNLOAD": "1",
    }
    if extra_env:
        env.update(extra_env)
    return subprocess.run(
        ["bash", str(INSTALL_SH)],
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )


def test_post_install_smoke_passes_when_binary_launches(tmp_path: Path) -> None:
    # Happy path: a binary that actually starts should produce an
    # installer exit 0 with the usual success messages.
    result = _install_with_stub(tmp_path, stub_behaviour="ok")
    assert result.returncode == 0, (
        f"installer failed: stdout={result.stdout!r} stderr={result.stderr!r}"
    )
    assert "Installed shipyard to" in result.stdout


def test_post_install_smoke_fails_loud_on_sigkill(tmp_path: Path) -> None:
    # The #219 failure mode: binary exists, is executable, passes
    # codesign verify on macOS — but dies at launch. The installer
    # MUST exit non-zero so downstream wrappers (pulp's
    # install-shipyard.sh, Spectr's, etc.) can abort instead of
    # claiming success and leaving the user with a dead binary.
    result = _install_with_stub(tmp_path, stub_behaviour="sigkill")
    assert result.returncode != 0, (
        "smoke test failure must propagate exit code; got 0 with "
        f"stdout={result.stdout!r} stderr={result.stderr!r}"
    )
    # Error message must be on stderr so wrapper scripts that redirect
    # stdout don't swallow it.
    assert "smoke test" in result.stderr.lower()
    # The #219 issue link is macOS-specific (taskgated doesn't exist
    # on Linux, and the .dmg-stapling fix is macOS-only). On Linux
    # the hint is generic "run the binary manually" — assert whichever
    # the current OS should emit. `test_post_install_smoke_remediation
    # _mentions_crash_report_on_macos` covers the macOS-specific text.
    if sys.platform == "darwin":
        assert "219" in result.stderr or "/issues/219" in result.stderr
    else:
        assert "run" in result.stderr.lower() and "manually" in result.stderr.lower()


def test_post_install_smoke_can_be_disabled(tmp_path: Path) -> None:
    # Escape hatch: CI or a wrapper that dispatches its own
    # verification can opt out via SHIPYARD_SKIP_SMOKE=1 so a
    # deliberately-broken stub doesn't prevent install-dir staging.
    result = _install_with_stub(
        tmp_path,
        stub_behaviour="sigkill",
        extra_env={"SHIPYARD_SKIP_SMOKE": "1"},
    )
    assert result.returncode == 0, (
        f"SHIPYARD_SKIP_SMOKE=1 must bypass smoke gate; got exit "
        f"{result.returncode} stderr={result.stderr!r}"
    )


def test_post_install_smoke_remediation_mentions_crash_report_on_macos(
    tmp_path: Path,
) -> None:
    # macOS-only: the remediation block should point at the
    # ~/Library/Logs/DiagnosticReports path so the user knows where
    # to look for the taskgated crash signature, not just "retry".
    # On Linux the hint is simpler so we conditionally assert.
    if sys.platform != "darwin":
        pytest.skip("macOS-specific remediation hint")
    # Disable ad-hoc fallback so we exercise the hard-fail path
    # (otherwise the sigkill stub would recover via fallback).
    result = _install_with_stub(
        tmp_path,
        stub_behaviour="sigkill",
        extra_env={"SHIPYARD_NO_ADHOC_FALLBACK": "1"},
    )
    assert result.returncode != 0
    assert "DiagnosticReports" in result.stderr
    assert "Code Signature Invalid" in result.stderr


# -- #219 take 2: ad-hoc fallback on taskgated rejection -----------
# install.sh now, on macOS, if the smoke probe fails AND the binary
# was Developer-ID signed, re-signs ad-hoc + retries. The user loses
# notarization trust but gains a launchable binary — strictly better
# than exit-1 and a dead install. Opt-out via
# SHIPYARD_NO_ADHOC_FALLBACK=1.

def _install_with_recovering_stub(
    tmp_path: Path,
    *,
    extra_env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    """Stub that SIGKILLs on first few calls, then succeeds once
    codesign has been invoked against it.

    Simulates the #219 fingerprint: the fresh notarized binary
    can't pass taskgated on this Mac, but an ad-hoc re-sign
    unblocks launch.

    Uses a sentinel sidecar file the stub mutates via `codesign`
    (observed via a codesign shim on PATH) — we can't really
    mutate the stub's own behaviour from within its process, so
    the pattern is: stub checks for sidecar; no sidecar →
    SIGKILL itself; sidecar present → print version.

    We point PATH at a fake codesign that writes the sidecar
    when invoked, so the install.sh ad-hoc-resign path flips
    the stub to working mode.
    """
    install_dir = tmp_path / "bin"
    install_dir.mkdir()
    stub = install_dir / "shipyard"
    sentinel = tmp_path / "adhoc-resigned"
    stub.write_text(
        "#!/bin/sh\n"
        f"if [ -f {sentinel!s} ]; then\n"
        "  echo shipyard 99.99.99\n"
        "  exit 0\n"
        "fi\n"
        "kill -KILL $$\n"
    )
    stub.chmod(0o755)

    # Fake codesign that:
    # - Responds to `codesign -dv` with a Developer-ID-ish output
    #   (TeamIdentifier line present) so install.sh classifies the
    #   stub as signed and attempts the fallback path.
    # - Responds to `codesign --force --sign -` by writing the
    #   sentinel, flipping the stub to "works now" mode.
    # - Responds to `codesign --remove-signature` as a no-op.
    shim_dir = tmp_path / "shim"
    shim_dir.mkdir()
    fake_codesign = shim_dir / "codesign"
    fake_codesign.write_text(
        "#!/bin/sh\n"
        'case "$*" in\n'
        '    *--force*--sign*-*)\n'
        f'        touch {sentinel!s}\n'
        '        exit 0 ;;\n'
        '    -dv*)\n'
        '        # install.sh greps the combined output for "^TeamIdentifier=".\n'
        '        # Write to BOTH streams so whichever the script\n'
        '        # captures gets a hit.\n'
        '        echo "TeamIdentifier=TESTTEAM"\n'
        '        echo "TeamIdentifier=TESTTEAM" >&2\n'
        '        exit 0 ;;\n'
        '    *--remove-signature*)\n'
        '        exit 0 ;;\n'
        '    *--verify*)\n'
        '        exit 0 ;;\n'
        '    *)\n'
        '        exit 0 ;;\n'
        'esac\n'
    )
    fake_codesign.chmod(0o755)

    env = {
        **os.environ,
        "SHIPYARD_INSTALL_DIR": str(install_dir),
        "SHIPYARD_SKIP_DOWNLOAD": "1",
        # Put the shim BEFORE the real codesign.
        "PATH": f"{shim_dir}:{os.environ.get('PATH', '')}",
    }
    if extra_env:
        env.update(extra_env)
    return subprocess.run(
        ["bash", str(INSTALL_SH)],
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )


def test_adhoc_fallback_recovers_from_taskgated_rejection(
    tmp_path: Path,
) -> None:
    # macOS only: the fallback branch is conditioned on OS=macos
    # since Linux doesn't have codesign / taskgated.
    if sys.platform != "darwin":
        pytest.skip("macOS-specific fallback path")
    result = _install_with_recovering_stub(tmp_path)
    assert result.returncode == 0, (
        "ad-hoc fallback should recover a taskgated-rejected binary; "
        f"got exit={result.returncode} stderr={result.stderr!r}"
    )
    # Fallback path must log the trade-off so the operator knows
    # Gatekeeper fast-path is now disabled on this install.
    assert "ad-hoc" in result.stderr.lower()
    assert "fast-path" in result.stderr.lower() or "fallback" in result.stderr.lower()


def test_adhoc_fallback_opt_out_fails_loud(tmp_path: Path) -> None:
    # Users who don't want ad-hoc (corp policy, prefer loud failure)
    # should get exit 1 + the hint that mentions how to re-enable.
    if sys.platform != "darwin":
        pytest.skip("macOS-specific fallback path")
    result = _install_with_recovering_stub(
        tmp_path,
        extra_env={"SHIPYARD_NO_ADHOC_FALLBACK": "1"},
    )
    assert result.returncode != 0
    assert "smoke test" in result.stderr.lower()
    # Must explain how to re-enable the fallback, so the user knows
    # their opt-out is what kept them dead-in-the-water.
    assert "SHIPYARD_NO_ADHOC_FALLBACK" in result.stderr


def test_adhoc_fallback_does_not_trigger_on_linux(tmp_path: Path) -> None:
    # Even if a Linux user somehow hits the smoke failure, install.sh
    # must NOT invoke codesign / ad-hoc resign (those are macOS tools).
    if sys.platform == "darwin":
        pytest.skip("Linux-only path")
    result = _install_with_stub(tmp_path, stub_behaviour="sigkill")
    assert result.returncode != 0
    # Ad-hoc messaging must be absent on Linux.
    assert "ad-hoc" not in result.stderr.lower()

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


def _platform_artifact_name() -> str:
    """Return the shipyard-<os>-<arch> artifact name for the current
    host, matching install.sh's ARTIFACT computation.

    Used by tests that shim curl: the fake release URL's filename
    must match the ARTIFACT regex install.sh greps against, and
    that's computed from ``uname`` at run time. Hardcoding
    ``shipyard-macos-arm64`` in the test breaks on Linux CI.
    """
    osname = "linux"
    if sys.platform == "darwin":
        osname = "macos"
    elif sys.platform == "win32":
        osname = "windows"
    import platform as _platform
    machine = _platform.machine().lower()
    if machine in {"arm64", "aarch64"}:
        arch = "arm64"
    else:
        arch = "x64"
    return f"shipyard-{osname}-{arch}"


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


# -- #52: .dmg mount-and-extract path ------------------------------
# install.sh on macOS downloads a stapled .dmg (new default in
# v0.44.0+), mounts it, copies the binary out. This test drives
# that path with a real dmg built by hdiutil + a stub binary that
# prints a known version string. Verifies:
#   - install.sh correctly detects the .dmg asset and takes the
#     mount path instead of the bare-Mach-O path
#   - After mount + copy + detach, the installed binary at
#     $INSTALL_DIR/shipyard is executable and prints the expected
#     content
#   - post-install smoke passes on the extracted binary

def _make_fake_dmg(tmp_path: Path, version_string: str = "shipyard, version test") -> Path:
    """Build a real stapled-shape .dmg with a stub shipyard inside.

    The dmg won't be *actually* stapled (no notarization ticket for
    a fake binary), but the mount + extract logic in install.sh is
    independent of the ticket — we're testing the mechanics, not
    the cryptography. Returns the dmg path.
    """
    if sys.platform != "darwin":
        raise RuntimeError("hdiutil is macOS-only")
    stage = tmp_path / "dmg-stage"
    stage.mkdir()
    stub = stage / "shipyard"
    stub.write_text(f"#!/bin/sh\necho '{version_string}'\n")
    stub.chmod(0o755)
    dmg = tmp_path / "shipyard-macos-arm64.dmg"
    subprocess.run(
        [
            "hdiutil", "create", "-volname", "Shipyard",
            "-srcfolder", str(stage), "-ov", "-format", "UDZO",
            str(dmg),
        ],
        check=True,
        capture_output=True,
    )
    return dmg


def test_install_sh_mounts_dmg_and_extracts_binary(tmp_path: Path) -> None:
    # macOS only — hdiutil + mount semantics don't exist on Linux
    # and the dmg path in install.sh only runs on OS=macos.
    if sys.platform != "darwin":
        pytest.skip("dmg path is macOS-only")

    dmg = _make_fake_dmg(tmp_path, version_string="shipyard, version dmg-test")
    install_dir = tmp_path / "bin"
    install_dir.mkdir()

    # Fake the GitHub API response by shimming curl. install.sh's
    # URL-resolution step calls curl twice: once for the API JSON,
    # once for the actual asset download. We return a json blob
    # that points at our local dmg (via file://) for both.
    shim_dir = tmp_path / "shim"
    shim_dir.mkdir()
    fake_curl = shim_dir / "curl"
    # First invocation (API JSON lookup) gets faked JSON that
    # contains a browser_download_url pointing at our local dmg.
    # Second invocation (the asset download) gets the real curl
    # which can handle file:// URLs. Use a marker file to track
    # which call we're on.
    fake_curl.write_text(f"""#!/bin/sh
REAL_CURL=/usr/bin/curl
MARKER={tmp_path}/curl-call-count
if [ ! -f "$MARKER" ]; then echo 0 > "$MARKER"; fi
COUNT=$(cat "$MARKER")
NEW=$((COUNT + 1))
echo "$NEW" > "$MARKER"
# Any call that mentions api.github.com: return a fake JSON with
# browser_download_url pointing at the local dmg. Any other call:
# pass through to real curl.
for arg in "$@"; do
    case "$arg" in
        *api.github.com*)
            echo '"browser_download_url": "file://{dmg}"'
            exit 0
            ;;
    esac
done
exec "$REAL_CURL" "$@"
""")
    fake_curl.chmod(0o755)

    env = {
        **os.environ,
        "SHIPYARD_INSTALL_DIR": str(install_dir),
        "PATH": f"{shim_dir}:{os.environ.get('PATH', '')}",
    }
    # Don't skip download — we're exercising the dmg mount path.
    env.pop("SHIPYARD_SKIP_DOWNLOAD", None)
    # Skip the smoke since it would codesign-probe the stub as if
    # it's a shipyard binary; the mount + extract mechanics are
    # what this test covers.
    env["SHIPYARD_SKIP_SMOKE"] = "1"

    result = subprocess.run(
        ["bash", str(INSTALL_SH)],
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, (
        f"dmg install failed: stdout={result.stdout!r} stderr={result.stderr!r}"
    )
    # The dmg path's download message must have fired, proving
    # install.sh took the dmg branch rather than bare Mach-O.
    assert ".dmg" in result.stdout, (
        f"expected .dmg download message; got stdout={result.stdout!r}"
    )
    # The extracted binary must exist and be executable.
    installed = install_dir / "shipyard"
    assert installed.exists(), f"binary missing: {list(install_dir.iterdir())}"
    assert os.access(installed, os.X_OK)
    # And it must contain what we put in the dmg — confirms the
    # copy actually happened, not just a pre-existing file.
    run = subprocess.run(
        [str(installed)], capture_output=True, text=True, check=False,
    )
    assert "dmg-test" in run.stdout, (
        f"extracted binary produced {run.stdout!r}; expected the "
        "dmg-test marker, which proves the binary came OUT of the dmg"
    )


def test_install_sh_matches_windows_exe_asset(tmp_path: Path) -> None:
    # Codex P1 on #227: my earlier RELEASE_URL grep anchored with
    # just `"` after `${ARTIFACT}`, which worked for Linux + macOS
    # bare Mach-O (where the asset name ends right before the
    # closing quote) but broke Windows — `shipyard-windows-x64.exe`
    # has `.exe` between ARTIFACT and the quote. The fix allows an
    # optional `.exe` suffix.
    #
    # Drive install.sh through the RELEASE_URL branch with a fake
    # windows-x64.exe asset. We can't actually RUN a .exe on
    # macOS/Linux, but we CAN verify the URL-resolution step picks
    # the right asset and gets through the download step. Skip the
    # final smoke since the stub isn't a real Windows binary.
    if sys.platform == "win32":
        pytest.skip("drives install.sh URL resolution; macOS+Linux enough")

    install_dir = tmp_path / "bin"
    install_dir.mkdir()

    # A fake Windows-named artifact. install.sh sees OS/ARCH from
    # `uname`, so ARTIFACT on a non-Windows test host is always
    # `shipyard-<os>-<arch>` (no .exe). To exercise the .exe match
    # path specifically we shim curl + force the grep target via
    # the fake JSON name — but install.sh computes ARTIFACT locally
    # from uname, so we also need the on-disk file to match the
    # local artifact name PLUS the .exe suffix case.
    #
    # Easier: build a test binary file that simulates a
    # platform-appropriate "windows-ish" asset name the grep would
    # see, and check install.sh follows it through to download.
    # We confirm the match by asserting install.sh writes the
    # downloaded bytes to ${INSTALL_DIR}/shipyard.
    platform_artifact = subprocess.run(
        ["bash", "-c",
         'case "$(uname -s)" in Darwin) echo "shipyard-macos-$(uname -m | sed s/x86_64/x64/;s/arm64/arm64/)";; '
         'Linux) echo "shipyard-linux-$(uname -m | sed s/x86_64/x64/;s/aarch64/arm64/)";; '
         'esac'],
        capture_output=True, text=True, check=True,
    ).stdout.strip()
    # We're going to pretend the release serves the platform artifact
    # with a `.exe` suffix — the grep must still match. We write a
    # file with .exe in the name and make the fake curl return a URL
    # pointing at it.
    exe_asset = tmp_path / f"{platform_artifact}.exe"
    exe_asset.write_text("fake-windows-payload\n")

    shim_dir = tmp_path / "shim"
    shim_dir.mkdir()
    fake_curl = shim_dir / "curl"
    fake_curl.write_text(f"""#!/bin/sh
REAL_CURL=/usr/bin/curl
for arg in "$@"; do
    case "$arg" in
        *api.github.com*)
            # Asset name carries .exe suffix. Before the fix this
            # line would NOT match the RELEASE_URL grep because the
            # grep anchored with just `"` right after ARTIFACT.
            echo '"browser_download_url": "file://{exe_asset}"'
            exit 0
            ;;
    esac
done
exec "$REAL_CURL" "$@"
""")
    fake_curl.chmod(0o755)

    env = {
        **os.environ,
        "SHIPYARD_INSTALL_DIR": str(install_dir),
        "PATH": f"{shim_dir}:{os.environ.get('PATH', '')}",
        "SHIPYARD_SKIP_SMOKE": "1",  # the fake payload isn't a real binary
    }
    env.pop("SHIPYARD_SKIP_DOWNLOAD", None)

    result = subprocess.run(
        ["bash", str(INSTALL_SH)],
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, (
        f".exe asset detection failed: stdout={result.stdout!r} "
        f"stderr={result.stderr!r}"
    )
    # The downloaded file must end up at ${INSTALL_DIR}/shipyard
    # with the payload we placed in exe_asset. If the grep failed
    # to match, install.sh would have exited with "No binary found"
    # before downloading anything.
    installed = install_dir / "shipyard"
    assert installed.exists(), "install.sh should have downloaded the .exe asset"
    assert installed.read_text() == "fake-windows-payload\n", (
        "downloaded content must match the .exe asset — proves the "
        "grep picked the .exe URL, not some other line"
    )


def test_install_sh_skips_download_when_already_at_target(tmp_path: Path) -> None:
    # #231: redundant pulp+spectr back-to-back installs were burning
    # ~15MB each time even when the target version matched what's
    # already on disk. install.sh now short-circuits the download
    # when VERSION_LABEL is a specific tag AND the existing binary
    # reports that exact version. This test proves no download
    # happens by shimming curl to drop a sentinel if called.
    install_dir = tmp_path / "bin"
    install_dir.mkdir()
    existing = install_dir / "shipyard"
    existing.write_text("#!/bin/sh\necho 'shipyard, version 0.46.0'\n")
    existing.chmod(0o755)

    shim_dir = tmp_path / "shim"
    shim_dir.mkdir()
    fake_curl = shim_dir / "curl"
    sentinel = tmp_path / "curl-was-called"
    fake_curl.write_text(f"""#!/bin/sh
touch {sentinel!s}
echo '"browser_download_url": "file://{existing}"'
""")
    fake_curl.chmod(0o755)

    env = {
        **os.environ,
        "SHIPYARD_INSTALL_DIR": str(install_dir),
        "SHIPYARD_VERSION": "v0.46.0",
        "PATH": f"{shim_dir}:{os.environ.get('PATH', '')}",
        "SHIPYARD_SKIP_SMOKE": "1",
    }
    env.pop("SHIPYARD_SKIP_DOWNLOAD", None)
    env.pop("SHIPYARD_FORCE_REINSTALL", None)

    result = subprocess.run(
        ["bash", str(INSTALL_SH)],
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, (
        f"idempotent install failed: stdout={result.stdout!r} "
        f"stderr={result.stderr!r}"
    )
    # Announcement so the operator sees the elision.
    assert "Already at v0.46.0" in result.stdout or \
           "skipping download" in result.stdout.lower()
    # Sentinel proves curl was never invoked.
    assert not sentinel.exists(), (
        "curl should not have been invoked when the binary is "
        "already at the target version"
    )


def test_install_sh_force_reinstall_overrides_idempotency(tmp_path: Path) -> None:
    # Escape hatch: SHIPYARD_FORCE_REINSTALL=1 re-downloads even when
    # the binary is already at the target.
    install_dir = tmp_path / "bin"
    install_dir.mkdir()
    existing = install_dir / "shipyard"
    existing.write_text("#!/bin/sh\necho 'shipyard, version 0.46.0'\n")
    existing.chmod(0o755)

    # The "release asset" must match install.sh's ARTIFACT regex
    # (`shipyard-<os>-<arch>`). Derive the name from the test
    # host's uname so this works on Linux CI + macOS dev alike.
    asset_src = tmp_path / _platform_artifact_name()
    asset_src.write_text("#!/bin/sh\necho 'shipyard, version 0.46.0'\n")
    asset_src.chmod(0o755)

    shim_dir = tmp_path / "shim"
    shim_dir.mkdir()
    fake_curl = shim_dir / "curl"
    sentinel = tmp_path / "curl-called"
    fake_curl.write_text(f"""#!/bin/sh
touch {sentinel!s}
echo '"browser_download_url": "file://{asset_src}"'
""")
    fake_curl.chmod(0o755)

    env = {
        **os.environ,
        "SHIPYARD_INSTALL_DIR": str(install_dir),
        "SHIPYARD_VERSION": "v0.46.0",
        "SHIPYARD_FORCE_REINSTALL": "1",
        "PATH": f"{shim_dir}:{os.environ.get('PATH', '')}",
        "SHIPYARD_SKIP_SMOKE": "1",
    }
    env.pop("SHIPYARD_SKIP_DOWNLOAD", None)

    result = subprocess.run(
        ["bash", str(INSTALL_SH)],
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, (
        f"--force-reinstall failed: stdout={result.stdout!r} "
        f"stderr={result.stderr!r}"
    )
    assert sentinel.exists(), (
        "SHIPYARD_FORCE_REINSTALL=1 must re-download even when "
        "version matches"
    )


def test_install_sh_idempotency_skipped_for_latest(tmp_path: Path) -> None:
    # VERSION_LABEL=latest requires a network round-trip to know
    # what "latest" resolves to, so short-circuit doesn't apply.
    # This test proves curl still gets called when targeting latest
    # even when the existing binary "looks recent."
    install_dir = tmp_path / "bin"
    install_dir.mkdir()
    existing = install_dir / "shipyard"
    existing.write_text("#!/bin/sh\necho 'shipyard, version 0.46.0'\n")
    existing.chmod(0o755)

    asset_src = tmp_path / _platform_artifact_name()
    asset_src.write_text("#!/bin/sh\necho 'shipyard, version 0.46.0'\n")
    asset_src.chmod(0o755)

    shim_dir = tmp_path / "shim"
    shim_dir.mkdir()
    fake_curl = shim_dir / "curl"
    sentinel = tmp_path / "curl-called"
    fake_curl.write_text(f"""#!/bin/sh
touch {sentinel!s}
echo '"browser_download_url": "file://{asset_src}"'
""")
    fake_curl.chmod(0o755)

    env = {
        **os.environ,
        "SHIPYARD_INSTALL_DIR": str(install_dir),
        "PATH": f"{shim_dir}:{os.environ.get('PATH', '')}",
        "SHIPYARD_SKIP_SMOKE": "1",
    }
    env.pop("SHIPYARD_SKIP_DOWNLOAD", None)
    env.pop("SHIPYARD_VERSION", None)  # defaults to latest
    env.pop("SHIPYARD_FORCE_REINSTALL", None)

    result = subprocess.run(
        ["bash", str(INSTALL_SH)],
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, (
        f"latest install failed: stdout={result.stdout!r} stderr={result.stderr!r}"
    )
    assert sentinel.exists(), (
        "VERSION_LABEL=latest must consult the network — "
        "can't short-circuit without resolving 'latest'"
    )


def test_install_sh_falls_back_to_bare_macho_when_no_dmg(tmp_path: Path) -> None:
    # Backward compat: if a tag has no .dmg asset (older releases,
    # forks without the local-sign script, etc.) install.sh must
    # still find the bare Mach-O and install it.
    if sys.platform != "darwin":
        pytest.skip("test exercises macOS-specific branch selection")

    install_dir = tmp_path / "bin"
    install_dir.mkdir()

    # Fake a Mach-O. The filename has to match the ARTIFACT grep
    # pattern install.sh runs (`browser_download_url.*${ARTIFACT}"`)
    # so that URL resolution finds it — use the real artifact name.
    bare_binary = tmp_path / "shipyard-macos-arm64"
    bare_binary.write_text("#!/bin/sh\necho 'shipyard, version bare-test'\n")
    bare_binary.chmod(0o755)

    shim_dir = tmp_path / "shim"
    shim_dir.mkdir()
    fake_curl = shim_dir / "curl"
    # API shim returns ONLY a bare-Mach-O URL (no dmg entry), so
    # install.sh's DMG_URL check comes up empty and it falls back
    # to RELEASE_URL.
    fake_curl.write_text(f"""#!/bin/sh
REAL_CURL=/usr/bin/curl
for arg in "$@"; do
    case "$arg" in
        *api.github.com*)
            # Only the bare artifact — no .dmg sibling. The trailing
            # double quote matches install.sh's RELEASE_URL grep
            # pattern (`browser_download_url.*${{ARTIFACT}}"`).
            echo '"browser_download_url": "file://{bare_binary}"'
            exit 0
            ;;
    esac
done
exec "$REAL_CURL" "$@"
""")
    fake_curl.chmod(0o755)

    env = {
        **os.environ,
        "SHIPYARD_INSTALL_DIR": str(install_dir),
        "PATH": f"{shim_dir}:{os.environ.get('PATH', '')}",
        "SHIPYARD_SKIP_SMOKE": "1",
    }
    env.pop("SHIPYARD_SKIP_DOWNLOAD", None)

    result = subprocess.run(
        ["bash", str(INSTALL_SH)],
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, (
        f"bare-fallback install failed: stdout={result.stdout!r} stderr={result.stderr!r}"
    )
    # Must NOT have taken the dmg mount path (no mention of .dmg
    # in the download message — bare path uses the artifact name).
    assert ".dmg" not in result.stdout, (
        f"expected bare-Mach-O path; got dmg-like stdout={result.stdout!r}"
    )
    installed = install_dir / "shipyard"
    assert installed.exists()
    run = subprocess.run(
        [str(installed)], capture_output=True, text=True, check=False,
    )
    assert "bare-test" in run.stdout

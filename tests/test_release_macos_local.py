"""Tests for scripts/release-macos-local.sh (#219 Option B).

Can't actually exercise codesign / notarytool / PyInstaller from
pytest — those require real Apple credentials + a signing identity
in the keychain. What we CAN exercise is:

- Help text renders
- Missing --tag fails with exit 2 and a clear message
- Missing env vars fail with exit 2 BEFORE any build work starts
  (critical: the build burns ~60s; failing fast on missing creds
  saves the operator that time)
- The script is syntactically valid bash
"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import pytest  # noqa: TC002 — used at runtime via MonkeyPatch fixture

pytestmark = pytest.mark.skipif(
    sys.platform == "win32",
    reason="release-macos-local.sh is a POSIX shell script; macOS-only in practice",
)

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "release-macos-local.sh"


def _run(
    args: list[str] | None = None,
    env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    merged = {**os.environ}
    # Clear any signing creds the developer happens to have set so
    # the test exercises the missing-env path deterministically.
    for var in (
        "SHIPYARD_NOTARIZE_APPLE_ID",
        "SHIPYARD_NOTARIZE_TEAM_ID",
        "SHIPYARD_NOTARIZE_APP_PASSWORD",
        "SHIPYARD_SIGNING_IDENTITY",
    ):
        merged.pop(var, None)
    if env:
        merged.update(env)
    return subprocess.run(
        ["bash", str(SCRIPT), *(args or [])],
        env=merged,
        capture_output=True,
        text=True,
        check=False,
    )


def test_script_exists_and_is_executable() -> None:
    assert SCRIPT.exists(), f"script not found at {SCRIPT}"
    assert os.access(SCRIPT, os.X_OK), "script must be executable"


def test_bash_syntax_valid() -> None:
    # `bash -n` parses without executing; catches typos that would
    # otherwise fail at release time when the stakes are higher.
    result = subprocess.run(
        ["bash", "-n", str(SCRIPT)],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, (
        f"bash -n failed: stdout={result.stdout!r} stderr={result.stderr!r}"
    )


def test_help_flag_renders_usage() -> None:
    result = _run(["--help"])
    assert result.returncode == 0
    # Help should name the required env vars so the user knows what
    # to set up before their first run.
    assert "SHIPYARD_NOTARIZE_APPLE_ID" in result.stdout
    assert "SHIPYARD_SIGNING_IDENTITY" in result.stdout
    assert "--upload" in result.stdout


def test_missing_tag_when_not_on_tagged_commit_exits_2() -> None:
    # On a branch that's not a tag, --tag must be explicit.
    # The test workspace is typically on a branch; if it happens to
    # be on a tagged commit this test is skipped — git describe
    # --exact-match succeeding means the user really is releasing.
    cwd_tag = subprocess.run(
        ["git", "describe", "--tags", "--exact-match"],
        capture_output=True,
        text=True,
        check=False,
    )
    if cwd_tag.returncode == 0:
        pytest.skip("test host HEAD is a tagged commit; --tag would default")

    result = _run()
    assert result.returncode == 2, (
        f"expected exit 2 on missing --tag; got {result.returncode} "
        f"stderr={result.stderr!r}"
    )
    assert "--tag required" in result.stderr or "not a tagged release" in result.stderr


def test_missing_env_var_fails_fast_before_build() -> None:
    # This is the load-bearing behavior: the PyInstaller build takes
    # ~60s and we must NOT start it if we're going to fail anyway
    # because creds aren't set. Failure must be exit 2 (input error)
    # not exit 1 (build error) so wrappers can distinguish.
    result = _run(["--tag", "v0.0.0-test"])
    assert result.returncode == 2, (
        f"expected exit 2 on missing env; got {result.returncode} "
        f"stderr={result.stderr!r}"
    )
    # The error must name at least one specific missing var so the
    # user knows what to set, not just "env var missing."
    assert "SHIPYARD_NOTARIZE_APPLE_ID" in result.stderr or \
           "SHIPYARD_NOTARIZE_TEAM_ID" in result.stderr or \
           "SHIPYARD_NOTARIZE_APP_PASSWORD" in result.stderr or \
           "SHIPYARD_SIGNING_IDENTITY" in result.stderr


def test_missing_env_var_error_points_to_script_header() -> None:
    # The header comment lists all four env vars + their purpose.
    # The error message should point the operator there instead of
    # dumping the full list inline (which rots when the list grows).
    result = _run(["--tag", "v0.0.0-test"])
    assert result.returncode == 2
    assert "release-macos-local.sh" in result.stderr


def test_all_env_vars_missing_names_first_missing_one_clearly() -> None:
    # If every env var is missing, the error should name ONE of them
    # first rather than dumping a concatenated blob. Predictable
    # single-line error is easier to grep in CI logs than a paragraph.
    result = _run(["--tag", "v0.0.0-test"])
    # Count "ERROR: ... is not set" lines; first one is the signal.
    error_lines = [
        line for line in result.stderr.splitlines()
        if "is not set in the environment" in line
    ]
    # At least one, but no more than one should be reported — the
    # script bails on the first missing var.
    assert len(error_lines) == 1, (
        f"expected exactly one missing-env error line; got {error_lines}"
    )


def test_script_documents_draft_until_complete_exit_4() -> None:
    # #252: script now flips the release from draft to public after
    # upload, and reverts to draft on E2E failure. Help text must
    # document exit 4 so operators + wrappers know what it means.
    content = SCRIPT.read_text()
    assert "4 " in content and "reverted to draft" in content, (
        "Exit code 4 must be documented in the script header"
    )
    # All nine step labels must be present — proxy test for the new
    # step 8 (publish) and step 9 (e2e) landing together. If someone
    # renumbers to 10 steps or drops a step this fires first.
    for n in range(1, 10):
        assert f"Step {n}/9" in content, f"missing Step {n}/9 label"


def test_release_yml_creates_draft_release_until_dmg_uploaded() -> None:
    # #252: release.yml must create the GitHub Release as a draft so
    # install.sh's `releases/latest` degrades to the previous
    # published release during the build/upload gap window.
    release_yml = REPO_ROOT / ".github" / "workflows" / "release.yml"
    content = release_yml.read_text()
    assert "draft: true" in content, (
        "release.yml must create new tag releases as draft — "
        "release-macos-local.sh flips draft=false after upload"
    )
    # Anchor the draft:true line to the softprops action block,
    # not a comment or stray YAML, by requiring both in the file.
    assert "softprops/action-gh-release" in content


def test_partial_arch_exit_path_does_not_reference_unset_vars() -> None:
    # Codex P1 on #254: the partial-arch exit path (only one of
    # arm64/x64 dmgs present) must not reference $E2E_TMPDIR — it's
    # only created later in step 9, and `set -u` is active, so
    # expanding it here raises an unbound-variable error and the
    # script exits non-zero instead of returning the intended "exit
    # 0, re-run for the other arch" contract.
    #
    # Doc-check rather than a runtime exercise because the partial
    # path is reached only via a real `gh release view` call; that
    # needs auth + a real release. Grep the source.
    content = SCRIPT.read_text()
    # Find the partial-arch block (starts at "Keeping release $TAG
    # as draft" and ends at "exit 0"). The block must NOT reference
    # any variable that's only created in step 9. E2E_TMPDIR is the
    # canonical one per #254; broaden the check if others surface.
    start = content.find("Keeping release $TAG as draft")
    assert start != -1, "partial-arch message block not found"
    end = content.find("exit 0", start)
    assert end != -1, "partial-arch exit 0 not found"
    partial_block = content[start:end]
    # Match actual variable expansions (`$E2E_TMPDIR` or
    # `${E2E_TMPDIR}`), not the word appearing inside a comment.
    import re
    matches = re.findall(r"\$\{?E2E_TMPDIR", partial_block)
    assert not matches, (
        "partial-arch exit path must not expand $E2E_TMPDIR — "
        "it's only set in step 9 and `set -u` would crash the script"
    )


def test_publish_gate_covers_arm64_only_after_intel_drop() -> None:
    # #256: Intel dropped as of v0.50.0. The arch-completeness gate
    # now expects only arm64. Regression guard: if someone re-adds
    # x64 to EXPECTED_MACOS_DMGS without also restoring the build
    # matrix + install.sh resolution, the gate would wait forever.
    content = SCRIPT.read_text()
    assert "EXPECTED_MACOS_DMGS" in content, (
        "script must express which macOS dmgs are expected before publish"
    )
    assert "shipyard-macos-arm64.dmg" in content
    assert "shipyard-macos-x64.dmg" not in content, (
        "x64 dropped in #256 — EXPECTED_MACOS_DMGS must be arm64 only"
    )
    # Revert-on-failure must track DID_PUBLISH, not just WAS_DRAFT —
    # otherwise a failed E2E right after this run's flip wouldn't
    # revert (WAS_DRAFT is 0 post-flip).
    assert "DID_PUBLISH" in content


def test_install_sh_rejects_intel_mac_cleanly() -> None:
    # #256: install.sh on Intel Mac must surface a clear "unsupported"
    # message and exit non-zero rather than fall through to a 404 on
    # the missing asset.
    install_sh = REPO_ROOT / "install.sh"
    content = install_sh.read_text()
    assert "Intel Macs (x86_64) are not supported" in content, (
        "install.sh must surface a clean unsupported-platform message "
        "for Intel Macs, not fall through to a 404 on a missing asset"
    )
    # Refusal must key on OS=macos + ARCH=x64, not on Linux x64.
    # Anchor to both so a future simplification of the condition
    # doesn't accidentally nuke Linux x64 installs.
    assert '$OS" = "macos"' in content and '$ARCH" = "x64"' in content


def test_intel_guard_runs_after_version_resolution() -> None:
    # Codex P1 on #257: if the Intel guard fires BEFORE version
    # resolution, users who explicitly pinned to a pre-drop tag that
    # still ships an Intel dmg (v0.44.0–v0.49.0) get wrongly refused.
    # The guard must be positioned AFTER the REQUESTED_VERSION →
    # VERSION_LABEL resolution block, and must gate on the resolved
    # version (latest OR >= v0.50.0) rather than unconditionally.
    install_sh = REPO_ROOT / "install.sh"
    content = install_sh.read_text()
    # Find both anchors and assert order.
    ver_resolution = content.find("# ── version resolution")
    intel_guard = content.find("Intel Macs (x86_64) are not supported")
    assert ver_resolution != -1, "version resolution block not found"
    assert intel_guard != -1, "Intel guard not found"
    assert ver_resolution < intel_guard, (
        "Intel guard must come AFTER version resolution — Codex P1 on "
        "#257: pre-drop pinned tags that still have Intel dmgs must "
        "still be installable via SHIPYARD_VERSION=vX.Y.Z"
    )
    # Guard must be version-conditional (latest or >= 0.50), not
    # unconditional for all macOS x64 invocations.
    assert "VERSION_LABEL" in content[intel_guard - 500:intel_guard]
    # Escape hatch (pinned older version) must be discoverable from
    # the message so operators know the workaround.
    assert "SHIPYARD_VERSION=v0.49.0" in content
    assert "v0.44.0" in content or "v0.49.0" in content


def test_release_yml_drops_macos_x64_matrix_row() -> None:
    # Pair with the script-side gate test above: the CI build matrix
    # must stop producing a macos-x64 artifact. Keeping it around
    # would waste CI minutes on an artifact that never gets signed
    # and which install.sh now refuses to resolve anyway.
    release_yml = REPO_ROOT / ".github" / "workflows" / "release.yml"
    content = release_yml.read_text()
    assert "target: macos-arm64" in content
    assert "target: macos-x64" not in content, (
        "#256: macos-x64 build matrix row must be gone; Intel dropped"
    )
    assert "macos_x64:" not in content, (
        "#256: macos_x64 runner output must be gone too"
    )

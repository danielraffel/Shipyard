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

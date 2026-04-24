"""Tests for doctor's `gh-scope` probe (#236).

`shipyard cloud retarget --apply` + `cloud handoff run --apply` need
`workflow` scope on the gh token to cancel runs. Pre-fix, the user
only discovered the missing scope mid-retarget with a bare
"Couldn't cancel the matching job(s)" error. The doctor probe
surfaces it before that happens.

Scope detection semantics covered here:
  - gh not installed → probe returns None (silent; the `gh` row
    already covers install state)
  - gh logged out → probe returns None (unrelated to scope)
  - classic PAT with workflow scope → ok=True
  - classic PAT without workflow scope → ok=False + fix command
  - fine-grained / GitHub App (no Token scopes line) → neutral pass
    with a clarifying message (scopes not inspectable locally)
"""

from __future__ import annotations

from types import SimpleNamespace
from typing import Any
from unittest.mock import patch

import pytest  # noqa: TC002 — MonkeyPatch fixture


def _ok_proc(
    returncode: int = 0,
    stdout: str = "",
    stderr: str = "",
) -> Any:
    return SimpleNamespace(returncode=returncode, stdout=stdout, stderr=stderr)


def _call() -> Any:
    from shipyard.cli import _check_gh_workflow_scope
    return _check_gh_workflow_scope()


def test_none_when_gh_not_installed(monkeypatch: pytest.MonkeyPatch) -> None:
    def fake_run(*args, **kw):
        raise FileNotFoundError("gh")

    with patch("shipyard.cli.subprocess.run", side_effect=fake_run):
        assert _call() is None


def test_none_when_gh_not_logged_in(monkeypatch: pytest.MonkeyPatch) -> None:
    # gh exits non-zero with "You are not logged in" when unauthed.
    def fake_run(*args, **kw):
        return _ok_proc(
            returncode=1, stderr="error: You are not logged in.",
        )

    with patch("shipyard.cli.subprocess.run", side_effect=fake_run):
        assert _call() is None


def test_ok_when_classic_pat_has_workflow_scope(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # gh's classic-PAT status output — real shape, stored on stderr
    # for gh versions pre-2.46 and split across stdout/stderr for
    # newer ones. We OR them when matching, so either works.
    stderr_text = (
        "github.com\n"
        "  ✓ Logged in to github.com account danielraffel (keyring)\n"
        "  - Active account: true\n"
        "  - Git operations protocol: https\n"
        "  - Token: gho_************************************\n"
        "  - Token scopes: 'gist', 'read:org', 'repo', 'workflow'\n"
    )
    with patch(
        "shipyard.cli.subprocess.run",
        side_effect=lambda *a, **k: _ok_proc(stderr=stderr_text),
    ):
        result = _call()
    assert result is not None
    assert result["ok"] is True
    assert "workflow" in result["version"].lower()


def test_flags_missing_workflow_scope_on_classic_pat(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Same shape as above but `workflow` is absent — the exact
    # state that bit the maintainer on pulp#711.
    stderr_text = (
        "github.com\n"
        "  ✓ Logged in to github.com account danielraffel (keyring)\n"
        "  - Token scopes: 'gist', 'read:org', 'repo'\n"
    )
    with patch(
        "shipyard.cli.subprocess.run",
        side_effect=lambda *a, **k: _ok_proc(stderr=stderr_text),
    ):
        result = _call()
    assert result is not None
    assert result["ok"] is False
    # Version line names the missing scope so `render_doctor` surfaces it.
    assert "workflow" in result["version"].lower()
    # Detail must include the refresh command — one-liner fix.
    assert "gh auth refresh -h github.com -s workflow" in result["detail"]
    # And a pointer for fine-grained / GitHub App identities so
    # users running under bots know where to look.
    assert "fine-grained" in result["detail"].lower() or \
           "install.md" in result["detail"].lower()


def test_neutral_pass_when_no_token_scopes_line(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Fine-grained tokens + GitHub Apps don't list scopes in
    # `gh auth status`. Classic-PAT grep misses → we neither
    # false-positive nor silently skip; return ok=True with a
    # clarifying version string so the operator knows we can't
    # verify locally.
    stderr_text = (
        "github.com\n"
        "  ✓ Logged in to github.com account danielraffel\n"
        "  - Token: github_pat_11ABCDEF...\n"
    )
    with patch(
        "shipyard.cli.subprocess.run",
        side_effect=lambda *a, **k: _ok_proc(stderr=stderr_text),
    ):
        result = _call()
    assert result is not None
    assert result["ok"] is True
    # Version string makes it clear we didn't verify.
    assert "fine-grained" in result["version"].lower() \
        or "not inspectable" in result["version"].lower()

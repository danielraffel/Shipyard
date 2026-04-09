"""Tests for `shipyard branch apply --create` and the underlying helpers."""

from __future__ import annotations

import subprocess
from unittest.mock import patch

from shipyard.governance.branch_create import (
    BranchCreateStatus,
    create_branch_and_apply_rules,
    create_branch_on_remote,
)
from shipyard.governance.github import GovernanceApiError, RepoRef
from shipyard.governance.profiles import BranchProtectionRules


def _ok(stdout: str = "", stderr: str = ""):
    return subprocess.CompletedProcess(
        args=[], returncode=0, stdout=stdout, stderr=stderr,
    )


def _fail(code: int, stderr: str = ""):
    return subprocess.CompletedProcess(
        args=[], returncode=code, stdout="", stderr=stderr,
    )


def _sample_rules() -> BranchProtectionRules:
    return BranchProtectionRules(
        require_pr=True,
        require_status_checks=("mac",),
        require_strict_status=False,
        require_review_count=0,
        enforce_admins=False,
    )


# ── create_branch_on_remote ─────────────────────────────────────────────


def test_create_branch_already_exists() -> None:
    """ls-remote returning 0 means the branch already exists."""
    with patch("subprocess.run", return_value=_ok(stdout="deadbeef\trefs/heads/develop/foo\n")):
        result = create_branch_on_remote(branch="develop/foo")
    assert result.status == BranchCreateStatus.ALREADY_EXISTS
    assert "already exists" in result.message


def test_create_branch_fresh_creation() -> None:
    """ls-remote returning 2 (ref not found) triggers the push path."""
    run_calls = []

    def fake_run(cmd, *args, **kwargs):
        run_calls.append(cmd)
        if "ls-remote" in cmd and "develop/foo" in cmd:
            return _fail(2)  # target branch not found
        if "ls-remote" in cmd and "refs/heads/main" in cmd:
            return _ok(stdout="abc123def456\trefs/heads/main\n")
        if "push" in cmd:
            return _ok(stderr="* [new branch] refs/heads/develop/foo")
        return _ok()

    with patch("subprocess.run", side_effect=fake_run):
        result = create_branch_on_remote(
            branch="develop/foo", base_branch="main",
        )
    assert result.status == BranchCreateStatus.CREATED
    assert "develop/foo" in result.message
    # Three git calls: exists-check, base-sha lookup, push
    assert len(run_calls) == 3
    assert "push" in run_calls[2]


def test_create_branch_pushes_sha_not_local_ref() -> None:
    """The push source must be the resolved SHA from ls-remote, not a local ref.

    Codex flagged that using `refs/remotes/origin/<base>` as the
    push source makes the command fail on shallow/single-branch
    clones (no tracking ref) and can create from a stale commit
    in long-lived worktrees.
    """
    push_cmd_seen: list = []

    def fake_run(cmd, *args, **kwargs):
        if "ls-remote" in cmd and "develop/foo" in cmd:
            return _fail(2)
        if "ls-remote" in cmd and "refs/heads/main" in cmd:
            return _ok(stdout="deadbeefcafe1234\trefs/heads/main\n")
        if "push" in cmd:
            push_cmd_seen.append(list(cmd))
            return _ok()
        return _ok()

    with patch("subprocess.run", side_effect=fake_run):
        result = create_branch_on_remote(
            branch="develop/foo", base_branch="main",
        )
    assert result.status == BranchCreateStatus.CREATED
    assert len(push_cmd_seen) == 1
    # The refspec source must be the concrete SHA, not a local ref
    refspec = push_cmd_seen[0][-1]
    assert refspec == "deadbeefcafe1234:refs/heads/develop/foo"
    # And it must NOT reference the local tracking ref at all
    for arg in push_cmd_seen[0]:
        assert "refs/remotes/origin" not in arg


def test_create_branch_base_lookup_empty_output() -> None:
    """Empty ls-remote output for the base branch returns GIT_FAILED, not crash."""

    def fake_run(cmd, *args, **kwargs):
        if "ls-remote" in cmd and "develop/foo" in cmd:
            return _fail(2)
        if "ls-remote" in cmd and "refs/heads/main" in cmd:
            return _ok(stdout="")
        return _ok()

    with patch("subprocess.run", side_effect=fake_run):
        result = create_branch_on_remote(
            branch="develop/foo", base_branch="main",
        )
    assert result.status == BranchCreateStatus.GIT_FAILED
    assert "no SHA" in result.message or "does the base branch exist" in result.message


def test_create_branch_base_lookup_fails() -> None:
    """A failing base-branch lookup returns GIT_FAILED with detail."""

    def fake_run(cmd, *args, **kwargs):
        if "ls-remote" in cmd and "develop/foo" in cmd:
            return _fail(2)
        if "ls-remote" in cmd and "refs/heads/main" in cmd:
            return _fail(128, stderr="fatal: Authentication failed")
        return _ok()

    with patch("subprocess.run", side_effect=fake_run):
        result = create_branch_on_remote(
            branch="develop/foo", base_branch="main",
        )
    assert result.status == BranchCreateStatus.GIT_FAILED
    assert "Authentication failed" in result.message


def test_create_branch_push_fails() -> None:
    def fake_run(cmd, *args, **kwargs):
        if "ls-remote" in cmd and "develop/foo" in cmd:
            return _fail(2)
        if "ls-remote" in cmd and "refs/heads/main" in cmd:
            return _ok(stdout="abc\trefs/heads/main\n")
        if "push" in cmd:
            return _fail(128, stderr="fatal: remote error")
        return _ok()

    with patch("subprocess.run", side_effect=fake_run):
        result = create_branch_on_remote(
            branch="develop/foo", base_branch="main",
        )
    assert result.status == BranchCreateStatus.GIT_FAILED
    assert "remote error" in result.message


def test_create_branch_ls_remote_unexpected_failure() -> None:
    """A non-2 ls-remote failure (auth, network) surfaces as GIT_FAILED."""
    with patch("subprocess.run", return_value=_fail(128, stderr="Permission denied")):
        result = create_branch_on_remote(branch="develop/foo")
    assert result.status == BranchCreateStatus.GIT_FAILED
    assert "Permission denied" in result.message


# ── create_branch_and_apply_rules ──────────────────────────────────────


def test_full_flow_creates_and_applies() -> None:
    def fake_run(cmd, *args, **kwargs):
        if "ls-remote" in cmd and "develop/foo" in cmd:
            return _fail(2)
        if "ls-remote" in cmd and "refs/heads/main" in cmd:
            return _ok(stdout="abc\trefs/heads/main\n")
        if "push" in cmd:
            return _ok()
        return _ok()

    with patch("subprocess.run", side_effect=fake_run), patch(
        "shipyard.governance.branch_create.put_branch_protection",
    ) as mock_put:
        result = create_branch_and_apply_rules(
            repo=RepoRef("me", "r"),
            branch="develop/foo",
            base_branch="main",
            rules=_sample_rules(),
        )
    assert result.status == BranchCreateStatus.RULES_APPLIED
    assert result.ok is True
    mock_put.assert_called_once()


def test_full_flow_branch_exists_still_applies_rules() -> None:
    """If the branch already exists, skip creation but still apply rules (idempotent)."""
    with patch(
        "subprocess.run",
        return_value=_ok(stdout="deadbeef\trefs/heads/develop/foo\n"),
    ), patch(
        "shipyard.governance.branch_create.put_branch_protection",
    ) as mock_put:
        result = create_branch_and_apply_rules(
            repo=RepoRef("me", "r"),
            branch="develop/foo",
            base_branch="main",
            rules=_sample_rules(),
        )
    assert result.status == BranchCreateStatus.RULES_APPLIED
    assert "already existed; reapplied" in result.message
    mock_put.assert_called_once()


def test_full_flow_rules_failure_leaves_branch_in_place() -> None:
    """A rules PUT failure must NOT delete the freshly-created branch."""

    def fake_run(cmd, *args, **kwargs):
        if "ls-remote" in cmd and "develop/foo" in cmd:
            return _fail(2)
        if "ls-remote" in cmd and "refs/heads/main" in cmd:
            return _ok(stdout="abc\trefs/heads/main\n")
        return _ok()

    with patch("subprocess.run", side_effect=fake_run), patch(
        "shipyard.governance.branch_create.put_branch_protection",
        side_effect=GovernanceApiError("permission denied"),
    ):
        result = create_branch_and_apply_rules(
            repo=RepoRef("me", "r"),
            branch="develop/foo",
            base_branch="main",
            rules=_sample_rules(),
        )
    assert result.status == BranchCreateStatus.RULES_FAILED
    assert result.ok is False
    assert "permission denied" in result.message
    assert "shipyard governance apply" in result.message  # retry hint


def test_full_flow_git_failure_aborts_early() -> None:
    """A git failure before rule apply skips the put entirely."""
    with patch(
        "subprocess.run",
        return_value=_fail(128, stderr="network unreachable"),
    ), patch(
        "shipyard.governance.branch_create.put_branch_protection",
    ) as mock_put:
        result = create_branch_and_apply_rules(
            repo=RepoRef("me", "r"),
            branch="develop/foo",
            base_branch="main",
            rules=_sample_rules(),
        )
    assert result.status == BranchCreateStatus.GIT_FAILED
    assert result.ok is False
    mock_put.assert_not_called()

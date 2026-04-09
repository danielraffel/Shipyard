"""Tests for the thin `gh api` wrappers (mocked).

No real HTTP or subprocess calls. Every test patches subprocess.run
and verifies the translation layer between GitHub's REST payloads
and BranchProtectionRules.
"""

from __future__ import annotations

import json
import subprocess
from unittest.mock import patch

import pytest

from shipyard.governance.github import (
    GovernanceApiError,
    RepoRef,
    _api_payload_from_rules,
    _parse_git_remote_url,
    _rules_from_api_payload,
    get_branch_protection,
    put_branch_protection,
)
from shipyard.governance.profiles import BranchProtectionRules


def _mock_run(returncode: int = 0, stdout: str = "", stderr: str = ""):
    return subprocess.CompletedProcess(
        args=[], returncode=returncode, stdout=stdout, stderr=stderr,
    )


# ── get_branch_protection ───────────────────────────────────────────────


def test_get_returns_none_on_404() -> None:
    with patch(
        "subprocess.run",
        return_value=_mock_run(returncode=1, stderr="Branch not protected"),
    ):
        assert get_branch_protection(RepoRef("me", "r"), "main") is None


def test_get_raises_on_permission_error() -> None:
    with patch(
        "subprocess.run",
        return_value=_mock_run(returncode=1, stderr="403 forbidden"),
    ), pytest.raises(GovernanceApiError, match="403"):
        get_branch_protection(RepoRef("me", "r"), "main")


def test_get_parses_valid_payload() -> None:
    payload = {
        "required_status_checks": {
            "strict": True,
            "contexts": ["macOS", "Linux"],
        },
        "required_pull_request_reviews": {
            "required_approving_review_count": 1,
            "dismiss_stale_reviews": True,
            "require_code_owner_reviews": False,
        },
        "enforce_admins": {"enabled": True},
        "allow_force_pushes": {"enabled": False},
        "allow_deletions": {"enabled": False},
        "required_linear_history": {"enabled": False},
        "required_conversation_resolution": {"enabled": True},
    }
    with patch(
        "subprocess.run",
        return_value=_mock_run(stdout=json.dumps(payload)),
    ):
        rules = get_branch_protection(RepoRef("me", "r"), "main")
    assert rules is not None
    assert rules.require_pr is True
    assert rules.require_strict_status is True
    assert set(rules.require_status_checks) == {"macOS", "Linux"}
    assert rules.require_review_count == 1
    assert rules.dismiss_stale_reviews is True
    assert rules.enforce_admins is True


def test_get_raises_on_malformed_json() -> None:
    with patch(
        "subprocess.run",
        return_value=_mock_run(stdout="not json"),
    ), pytest.raises(GovernanceApiError, match="non-JSON"):
        get_branch_protection(RepoRef("me", "r"), "main")


def test_get_raises_on_timeout() -> None:
    with patch(
        "subprocess.run",
        side_effect=subprocess.TimeoutExpired(cmd="gh", timeout=30),
    ), pytest.raises(GovernanceApiError, match="timeout"):
        get_branch_protection(RepoRef("me", "r"), "main")


# ── put_branch_protection ───────────────────────────────────────────────


def test_put_issues_api_call() -> None:
    rules = BranchProtectionRules(
        require_pr=True,
        require_status_checks=("mac", "linux"),
        require_strict_status=True,
        require_review_count=1,
        enforce_admins=True,
    )
    with patch("subprocess.run", return_value=_mock_run()) as mock_run:
        put_branch_protection(RepoRef("me", "r"), "main", rules)
    call = mock_run.call_args
    cmd = call[0][0]
    assert cmd[0:4] == ["gh", "api", "-X", "PUT"]
    assert "repos/me/r/branches/main/protection" in cmd
    # The body should contain the rule fields
    body = json.loads(call[1]["input"])
    assert body["required_status_checks"]["strict"] is True
    assert body["required_status_checks"]["contexts"] == ["mac", "linux"]
    assert body["enforce_admins"] is True


def test_put_raises_on_failure() -> None:
    rules = BranchProtectionRules(require_pr=True)
    with patch(
        "subprocess.run",
        return_value=_mock_run(returncode=1, stderr="permission denied"),
    ), pytest.raises(GovernanceApiError, match="permission"):
        put_branch_protection(RepoRef("me", "r"), "main", rules)


def test_put_omits_status_checks_block_when_empty() -> None:
    """GitHub rejects an empty contexts list, so omit the whole block."""
    rules = BranchProtectionRules(require_pr=True, require_status_checks=())
    with patch("subprocess.run", return_value=_mock_run()) as mock_run:
        put_branch_protection(RepoRef("me", "r"), "main", rules)
    body = json.loads(mock_run.call_args[1]["input"])
    assert body["required_status_checks"] is None


# ── payload translation round-trip ─────────────────────────────────────


def test_payload_to_rules_and_back_preserves_fields() -> None:
    original = BranchProtectionRules(
        require_pr=True,
        require_status_checks=("mac",),
        require_strict_status=False,
        require_review_count=2,
        enforce_admins=True,
        dismiss_stale_reviews=True,
        allow_force_push=False,
        allow_deletions=False,
        required_conversation_resolution=True,
    )
    payload = _api_payload_from_rules(original)
    # Build a GitHub-like response shape
    live_shape = {
        "required_status_checks": payload["required_status_checks"],
        "required_pull_request_reviews": payload["required_pull_request_reviews"],
        "enforce_admins": {"enabled": payload["enforce_admins"]},
        "allow_force_pushes": {"enabled": payload["allow_force_pushes"]},
        "allow_deletions": {"enabled": payload["allow_deletions"]},
        "required_linear_history": {"enabled": payload["required_linear_history"]},
        "required_conversation_resolution": {
            "enabled": payload["required_conversation_resolution"],
        },
    }
    round_tripped = _rules_from_api_payload(live_shape)
    assert round_tripped == original


# ── _parse_git_remote_url ──────────────────────────────────────────────


def test_parse_https_remote() -> None:
    ref = _parse_git_remote_url("https://github.com/danielraffel/pulp.git")
    assert ref == RepoRef(owner="danielraffel", name="pulp")


def test_parse_ssh_remote() -> None:
    ref = _parse_git_remote_url("git@github.com:danielraffel/pulp.git")
    assert ref == RepoRef(owner="danielraffel", name="pulp")


def test_parse_https_remote_without_dot_git() -> None:
    ref = _parse_git_remote_url("https://github.com/danielraffel/pulp")
    assert ref == RepoRef(owner="danielraffel", name="pulp")


def test_parse_non_github_remote_returns_none() -> None:
    assert _parse_git_remote_url("https://gitlab.com/foo/bar.git") is None

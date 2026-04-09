"""Regression: apply_bundle must fetch into a Shipyard-owned namespace.

The original implementation fetched `+refs/*:refs/*`, which triggers
the "refusing to fetch into branch X checked out at <path>" error
whenever the remote worktree happens to have a bundled branch
checked out — extremely common on long-lived validation VMs where a
stale `feature/*` branch is left checked out between runs.

Fetching into `refs/shipyard-bundles/*` instead bypasses git's
checked-out-ref safety entirely, because the destination is never a
worktree ref.
"""

from __future__ import annotations

import subprocess
from unittest.mock import patch

from shipyard.bundle.git_bundle import apply_bundle


def test_apply_bundle_uses_namespaced_refspec() -> None:
    """The remote fetch refspec must NOT overwrite refs/* directly."""
    captured: list[str] = []

    def fake_run(cmd, *args, **kwargs):
        captured.append(" ".join(cmd) if isinstance(cmd, list) else str(cmd))
        return subprocess.CompletedProcess(
            args=[], returncode=0, stdout="", stderr="",
        )

    with patch("subprocess.run", side_effect=fake_run):
        result = apply_bundle(
            host="ubuntu",
            bundle_path="/tmp/shipyard.bundle",
            repo_path="/home/x/repo",
        )
    assert result.success is True
    # The command must route through a shipyard-owned namespace
    assert len(captured) == 1
    cmd = captured[0]
    assert "refs/shipyard-bundles/heads/*" in cmd
    assert "refs/shipyard-bundles/tags/*" in cmd
    # And must NOT use the unsafe blanket refs/* mapping
    assert "refs/*:refs/*" not in cmd


def test_apply_bundle_verifies_before_fetching() -> None:
    """Verify must run before fetch so a broken bundle fails cleanly."""
    captured: list[str] = []

    def fake_run(cmd, *args, **kwargs):
        captured.append(" ".join(cmd) if isinstance(cmd, list) else str(cmd))
        return subprocess.CompletedProcess(
            args=[], returncode=0, stdout="", stderr="",
        )

    with patch("subprocess.run", side_effect=fake_run):
        apply_bundle(
            host="ubuntu",
            bundle_path="/tmp/shipyard.bundle",
            repo_path="/home/x/repo",
        )
    assert "git bundle verify" in captured[0]
    # verify must appear before fetch in the same command
    verify_pos = captured[0].find("git bundle verify")
    fetch_pos = captured[0].find("git fetch")
    assert 0 <= verify_pos < fetch_pos

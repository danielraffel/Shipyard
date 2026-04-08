"""PR operations via gh CLI.

Wraps GitHub CLI commands for creating, merging, and querying pull requests.
All functions shell out to `gh` and return structured results.
"""

from __future__ import annotations

import json
import subprocess
from dataclasses import dataclass
from typing import Any


class GhError(Exception):
    """Raised when a gh CLI command fails."""

    def __init__(self, message: str, returncode: int = 1) -> None:
        super().__init__(message)
        self.returncode = returncode


@dataclass(frozen=True)
class PrInfo:
    """Structured PR metadata."""

    number: int
    url: str
    title: str
    state: str
    branch: str
    base: str
    mergeable: str | None = None
    checks_passing: bool | None = None

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "number": self.number,
            "url": self.url,
            "title": self.title,
            "state": self.state,
            "branch": self.branch,
            "base": self.base,
        }
        if self.mergeable is not None:
            d["mergeable"] = self.mergeable
        if self.checks_passing is not None:
            d["checks_passing"] = self.checks_passing
        return d


def create_pr(
    branch: str,
    base: str,
    title: str,
    body: str,
) -> PrInfo:
    """Create a pull request via gh CLI.

    Pushes the branch first if it has no upstream, then creates the PR.
    Returns structured PR info on success.
    """
    # Push branch to remote
    _run_git(["git", "push", "-u", "origin", branch])

    # Create the PR
    result = _run_gh([
        "gh", "pr", "create",
        "--head", branch,
        "--base", base,
        "--title", title,
        "--body", body,
    ])

    # gh pr create prints the URL on success
    pr_url = result.strip().splitlines()[-1].strip()

    # Fetch full PR info
    return get_pr_status(pr_url)


def merge_pr(pr_number: int) -> PrInfo:
    """Merge a PR via gh CLI using merge commit strategy.

    Returns the PR info after merge.
    """
    _run_gh([
        "gh", "pr", "merge", str(pr_number),
        "--merge",
        "--delete-branch",
    ])

    return get_pr_status(str(pr_number))


def get_pr_status(pr_number_or_url: str) -> PrInfo:
    """Get current PR status including checks.

    Accepts a PR number or full URL.
    """
    result = _run_gh([
        "gh", "pr", "view", pr_number_or_url,
        "--json", "number,url,title,state,headRefName,baseRefName,mergeable,statusCheckRollup",
    ])

    data = json.loads(result)

    # Determine if all checks are passing
    checks = data.get("statusCheckRollup") or []
    checks_passing: bool | None = None
    if checks:
        checks_passing = all(
            c.get("conclusion") == "SUCCESS" or c.get("state") == "SUCCESS"
            for c in checks
        )

    return PrInfo(
        number=data["number"],
        url=data["url"],
        title=data["title"],
        state=data["state"],
        branch=data["headRefName"],
        base=data["baseRefName"],
        mergeable=data.get("mergeable"),
        checks_passing=checks_passing,
    )


def find_pr_for_branch(branch: str) -> PrInfo | None:
    """Find an existing open PR for the given branch.

    Returns None if no open PR exists.
    """
    try:
        result = _run_gh([
            "gh", "pr", "list",
            "--head", branch,
            "--state", "open",
            "--json", "number,url,title,state,headRefName,baseRefName",
            "--limit", "1",
        ])
        data = json.loads(result)
        if not data:
            return None

        pr = data[0]
        return PrInfo(
            number=pr["number"],
            url=pr["url"],
            title=pr["title"],
            state=pr["state"],
            branch=pr["headRefName"],
            base=pr["baseRefName"],
        )
    except GhError:
        return None


def _run_gh(cmd: list[str]) -> str:
    """Run a gh CLI command and return stdout."""
    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=60,
        )
        if result.returncode != 0:
            msg = result.stderr.strip() or result.stdout.strip() or "gh command failed"
            raise GhError(msg, result.returncode)
        return result.stdout
    except FileNotFoundError:
        raise GhError("gh CLI not installed. Install from https://cli.github.com/")
    except subprocess.TimeoutExpired:
        raise GhError("gh command timed out after 60 seconds")


def _run_git(cmd: list[str]) -> str:
    """Run a git command and return stdout."""
    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=30,
        )
        if result.returncode != 0:
            msg = result.stderr.strip() or "git command failed"
            raise GhError(msg, result.returncode)
        return result.stdout
    except FileNotFoundError:
        raise GhError("git not found")

"""Shared fixtures for changelog tests.

We seed real tiny git repositories (no mocking of git plumbing). The
fixtures pin author/committer identity + dates so generator output is
deterministic across runs and CI hosts.
"""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest


@pytest.fixture(autouse=True)
def _pin_git_env(monkeypatch: pytest.MonkeyPatch) -> None:
    """Pin git identity for every test in this subtree."""
    monkeypatch.setenv("GIT_AUTHOR_NAME", "Test Author")
    monkeypatch.setenv("GIT_AUTHOR_EMAIL", "test@example.com")
    monkeypatch.setenv("GIT_COMMITTER_NAME", "Test Author")
    monkeypatch.setenv("GIT_COMMITTER_EMAIL", "test@example.com")


def git(args: list[str], cwd: Path, *, env: dict[str, str] | None = None) -> str:
    """Run a git command with merged env and return stripped stdout."""
    merged = dict(os.environ)
    if env:
        merged.update(env)
    return subprocess.check_output(
        ["git", *args],
        cwd=str(cwd),
        text=True,
        env=merged,
        stderr=subprocess.STDOUT,
    ).strip()


def seed_repo(path: Path) -> Path:
    """Initialize an empty git repo on branch ``main``."""
    path.mkdir(parents=True, exist_ok=True)
    git(["init", "--quiet", "--initial-branch=main"], path)
    git(["config", "commit.gpgsign", "false"], path)
    return path


def commit(
    path: Path,
    filename: str,
    content: str,
    message: str,
    *,
    date: str = "2026-01-01T00:00:00+00:00",
) -> str:
    """Write ``filename`` with ``content`` and commit with fixed dates."""
    (path / filename).write_text(content)
    git(["add", filename], path)
    env = {
        "GIT_AUTHOR_DATE": date,
        "GIT_COMMITTER_DATE": date,
    }
    git(["commit", "-m", message], path, env=env)
    return git(["rev-parse", "HEAD"], path)


def tag(path: Path, name: str, *, date: str = "2026-01-01T00:00:00+00:00") -> None:
    """Annotated tag at HEAD with a pinned committer date."""
    env = {
        "GIT_AUTHOR_DATE": date,
        "GIT_COMMITTER_DATE": date,
    }
    git(["tag", "-a", name, "-m", f"Release {name}"], path, env=env)

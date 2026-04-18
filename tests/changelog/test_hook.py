"""Hook runner tests — real git repo + local bare remote.

We simulate the workflow's environment: a bare remote, a working clone,
and a mutator command that the hook runs. We then verify:

- Watched file diffs trigger a commit with ``[skip ci]`` + trailers.
- Unwatched diffs are ignored.
- A concurrent push on the remote triggers rebase-retry and still
  ultimately lands.
- Command failure aborts cleanly without pushing.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from shipyard.changelog.hook import HookConfig, run_hook

from tests.changelog.conftest import commit, git, seed_repo


def _make_remote_clone(tmp_path: Path) -> tuple[Path, Path]:
    """Create a bare remote + a working clone pointing at it."""
    remote = tmp_path / "remote.git"
    remote.mkdir()
    git(["init", "--quiet", "--bare", "--initial-branch=main"], remote)

    work = seed_repo(tmp_path / "work")
    commit(work, "README", "hello\n", "initial (#1)")
    git(["remote", "add", "origin", str(remote)], work)
    git(["push", "origin", "main"], work)
    git(["fetch", "origin"], work)
    git(["branch", "--set-upstream-to=origin/main", "main"], work)
    return remote, work


def _hook_cfg(command: str, watch: tuple[str, ...] = ("CHANGELOG.md",)) -> HookConfig:
    return HookConfig(
        enabled=True,
        command=command,
        watch=watch,
        trailers=(
            'Version-Bump: sdk=skip reason="docs-only"',
            'Skill-Update: skip skill=ci reason="none"',
            'Release: skip reason="bot commit"',
        ),
        only_for_tag_pattern="v*",
        max_push_attempts=3,
        bot_name="shipyard-test-bot",
        bot_email="bot@example.com",
    )


def test_disabled_hook_is_noop(tmp_path: Path) -> None:
    _, work = _make_remote_clone(tmp_path)
    cfg = HookConfig(enabled=False)
    result = run_hook(cfg, "v0.1.0", cwd=work)
    assert result.skipped_reason == "hook disabled in config"
    assert not result.ran_command


def test_tag_pattern_mismatch_skips(tmp_path: Path) -> None:
    _, work = _make_remote_clone(tmp_path)
    cfg = _hook_cfg(command="echo noop")
    result = run_hook(cfg, "plugin-v0.1.0", cwd=work)
    assert result.skipped_reason is not None
    assert "plugin-v0.1.0" in result.skipped_reason


def test_no_watched_diff_is_success_without_commit(tmp_path: Path) -> None:
    _, work = _make_remote_clone(tmp_path)
    # Command writes an UNWATCHED file.
    cfg = _hook_cfg(command="echo ignored > unrelated.txt", watch=("CHANGELOG.md",))
    result = run_hook(cfg, "v0.1.0", cwd=work)
    assert result.ran_command
    assert result.command_exit == 0
    assert not result.committed
    assert not result.pushed
    assert result.error is None


def test_watched_diff_commits_and_pushes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    remote, work = _make_remote_clone(tmp_path)

    # After the seed repo is set up, drop the env pins so the hook's
    # `git config user.*` calls actually take effect. GIT_AUTHOR_NAME
    # environment variables always win over `git config` values.
    for var in (
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
    ):
        monkeypatch.delenv(var, raising=False)
    cfg = _hook_cfg(
        command="printf '%s\\n' '# Changelog' 'new content' > CHANGELOG.md",
        watch=("CHANGELOG.md",),
    )
    result = run_hook(cfg, "v0.1.0", cwd=work)
    assert result.committed, result.error
    assert result.pushed, result.error
    assert result.attempts == 1

    body = git(["log", "-1", "--format=%B", "main"], work)
    assert "[skip ci]" in body
    assert "Version-Bump: sdk=skip" in body
    assert "Skill-Update: skip" in body
    assert "Release: skip" in body

    # Bot identity applied.
    author = git(["log", "-1", "--format=%an", "main"], work)
    assert author == "shipyard-test-bot"

    # Remote received the commit.
    remote_sha = git(["rev-parse", "main"], remote)
    local_sha = git(["rev-parse", "HEAD"], work)
    assert remote_sha == local_sha


def test_rebase_retry_after_concurrent_push(tmp_path: Path) -> None:
    """Seed a concurrent commit on the remote between our command and push.

    The workflow: the hook's command runs, produces a watched diff,
    commits. We then simulate another PR landing on the remote by
    pushing a separate commit directly to the bare repo's main. When
    ``run_hook`` tries to push, it'll hit non-fast-forward, rebase, and
    retry.
    """
    remote, work = _make_remote_clone(tmp_path)

    # Make a sibling clone so we can land an "other PR" directly on the
    # remote while ``work`` is still on the old tip.
    other = tmp_path / "other"
    git(["clone", str(remote), str(other)], tmp_path)
    # Ensure committer identity is set in the sibling clone (git env
    # vars propagate from parent process so this is covered).
    commit(other, "SIBLING.md", "sibling\n", "feat: parallel (#9)")
    git(["push", "origin", "main"], other)

    cfg = _hook_cfg(
        command="printf '%s\\n' '# Changelog' 'hook content' > CHANGELOG.md",
        watch=("CHANGELOG.md",),
    )
    # Fake sleep so tests are fast.
    result = run_hook(cfg, "v0.1.0", cwd=work, sleep=lambda _: None)
    assert result.pushed, f"push never landed: error={result.error}"

    # Remote main now has the sibling commit AND the hook commit.
    remote_log = git(["log", "main", "--format=%s"], remote)
    assert "feat: parallel (#9)" in remote_log
    assert any(
        "regenerate changelog" in line for line in remote_log.splitlines()
    )


def test_command_failure_aborts_cleanly(tmp_path: Path) -> None:
    _, work = _make_remote_clone(tmp_path)
    cfg = _hook_cfg(command="exit 17")
    result = run_hook(cfg, "v0.1.0", cwd=work)
    assert result.ran_command
    assert result.command_exit == 17
    assert not result.committed
    assert not result.pushed
    assert result.error is not None and "exit 17" in result.error.replace(
        "exited 17", "exit 17"
    )


def test_watched_multiple_files(tmp_path: Path) -> None:
    _, work = _make_remote_clone(tmp_path)
    cfg = _hook_cfg(
        command=(
            "printf 'cl\\n' > CHANGELOG.md && "
            "printf 'rn\\n' > RELEASE_NOTES.md"
        ),
        watch=("CHANGELOG.md", "RELEASE_NOTES.md"),
    )
    result = run_hook(cfg, "v0.1.0", cwd=work)
    assert result.pushed
    assert set(result.watched_diffed) == {"CHANGELOG.md", "RELEASE_NOTES.md"}

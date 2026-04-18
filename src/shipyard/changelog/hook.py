"""Post-tag command runner.

Mirrors pulp's ``auto-release.yml`` post-tag block as a reusable
module:

1. Shell out to ``cfg.command`` (e.g. ``shipyard changelog regenerate``
   or a custom script).
2. Check if any path in ``cfg.watch`` has an unstaged diff.
3. If yes: stage watched files, commit with configured trailers +
   ``[skip ci]``, rebase on to ``origin/main``, push.
4. On rebase conflict, abort the rebase, hard-reset to ``origin/main``,
   re-run the command, re-commit. Up to ``max_push_attempts`` tries.

Best-effort semantics per the spec: if the tag was already pushed
successfully, docs sync failure does not roll back the release.
"""

from __future__ import annotations

import fnmatch
import os
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Callable, Iterable

    from shipyard.core.config import Config

DEFAULT_TRAILERS: tuple[str, ...] = (
    'Version-Bump: sdk=skip reason="docs-only automated regeneration"',
    'Skill-Update: skip skill=ci reason="no workflow shape change"',
    'Release: skip reason="bot commit; prevent recursive auto-release"',
)


@dataclass
class HookConfig:
    """Parameters for the post-tag runner. Loaded from ``[release.post_tag_hook]``."""

    enabled: bool = False
    command: str = "shipyard changelog regenerate"
    watch: tuple[str, ...] = ("CHANGELOG.md",)
    trailers: tuple[str, ...] = DEFAULT_TRAILERS
    only_for_tag_pattern: str = "v*"
    max_push_attempts: int = 5
    bot_name: str = "shipyard-release-bot"
    bot_email: str = "shipyard-release-bot@users.noreply.github.com"
    remote: str = "origin"
    branch: str = "main"


@dataclass
class HookResult:
    """Outcome of a single ``run_hook`` invocation."""

    ran_command: bool = False
    command_exit: int = 0
    watched_diffed: list[str] = field(default_factory=list)
    committed: bool = False
    pushed: bool = False
    attempts: int = 0
    skipped_reason: str | None = None
    error: str | None = None


def load_hook_config(config: Config) -> HookConfig:
    """Extract ``[release.post_tag_hook]`` from a loaded shipyard Config."""
    section = config.get("release.post_tag_hook")
    if not isinstance(section, dict):
        return HookConfig(enabled=False)

    watch = section.get("watch")
    watch_tuple = tuple(str(x) for x in watch) if isinstance(watch, list) else ("CHANGELOG.md",)

    trailers = section.get("trailers")
    trailer_tuple = (
        tuple(str(x) for x in trailers)
        if isinstance(trailers, list) and trailers
        else DEFAULT_TRAILERS
    )

    bot_identity = section.get("bot_identity") or {}
    if not isinstance(bot_identity, dict):
        bot_identity = {}

    return HookConfig(
        enabled=bool(section.get("enabled", False)),
        command=str(section.get("command", "shipyard changelog regenerate")),
        watch=watch_tuple,
        trailers=trailer_tuple,
        only_for_tag_pattern=str(section.get("only_for_tag_pattern", "v*")),
        max_push_attempts=int(section.get("max_push_attempts", 5)),
        bot_name=str(bot_identity.get("name", "shipyard-release-bot")),
        bot_email=str(
            bot_identity.get("email", "shipyard-release-bot@users.noreply.github.com")
        ),
        remote=str(section.get("remote", "origin")),
        branch=str(section.get("branch", "main")),
    )


# ---- low-level git helpers ---------------------------------------


def _git(args: list[str], cwd: Path, *, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["git", *args],
        cwd=str(cwd),
        check=check,
        text=True,
        capture_output=True,
    )


def _has_unstaged_diff(paths: Iterable[str], cwd: Path) -> list[str]:
    """Return the subset of ``paths`` that differ from HEAD.

    Covers three cases:

    - Tracked, unstaged diff (``git diff``)
    - Tracked, staged diff (``git diff --cached``)
    - Untracked file that matches a watched path (``git status --porcelain``)
    """
    diffed: list[str] = []
    # Untracked-aware status for anything that isn't yet in HEAD.
    status = subprocess.run(
        ["git", "status", "--porcelain", "--", *paths],
        cwd=str(cwd),
        check=False,
        text=True,
        capture_output=True,
    ).stdout
    status_paths: set[str] = set()
    for line in status.splitlines():
        if len(line) >= 3:
            # Porcelain format: "XY path"; strip the two status chars.
            status_paths.add(line[3:].split(" -> ")[-1].strip())

    for p in paths:
        if p in status_paths:
            diffed.append(p)
            continue
        rc = subprocess.run(
            ["git", "diff", "--quiet", "HEAD", "--", p],
            cwd=str(cwd),
            check=False,
        ).returncode
        if rc != 0:
            diffed.append(p)
    return diffed


def _tag_matches(tag: str, pattern: str) -> bool:
    """Shell-glob match — mirrors git's ``--list`` glob semantics."""
    return fnmatch.fnmatchcase(tag, pattern)


def _build_commit_message(tag: str, trailers: Iterable[str]) -> list[str]:
    """Return a ``git commit -m ... -m ...`` arg list."""
    subject = f"docs: regenerate changelog for {tag} [skip ci]"
    body = (
        "Automated by shipyard release-bot hook run after tag push, so "
        "CHANGELOG.md and the GitHub Release page stay in sync."
    )
    args: list[str] = ["-m", subject, "-m", body, "-m", ""]
    for t in trailers:
        args.extend(["-m", t])
    return args


# ---- public entry point ------------------------------------------


def run_hook(
    cfg: HookConfig,
    tag: str,
    *,
    cwd: Path | None = None,
    env: dict[str, str] | None = None,
    sleep: Callable[[float], None] = time.sleep,
) -> HookResult:
    """Execute the post-tag hook for ``tag``.

    On SUCCESS the result has ``pushed=True`` (or ``committed=False`` if
    the command produced no watched-file diff). On FAILURE the result
    captures the failure reason but does NOT raise — callers can choose
    to propagate or swallow per their best-effort policy.

    Contract:

    - If ``cfg.enabled`` is False → no-op.
    - If ``tag`` doesn't match ``cfg.only_for_tag_pattern`` → no-op.
    - If ``cfg.command`` exits non-zero → abort, report error.
    - If no watched file diffed → success, ``committed=False``.
    - On non-fast-forward push, rebase on ``origin/<branch>`` and retry
      up to ``cfg.max_push_attempts`` times.
    """
    result = HookResult()
    base = cwd or Path.cwd()

    if not cfg.enabled:
        result.skipped_reason = "hook disabled in config"
        return result

    if not _tag_matches(tag, cfg.only_for_tag_pattern):
        result.skipped_reason = f"tag {tag!r} does not match {cfg.only_for_tag_pattern!r}"
        return result

    shell_env = dict(os.environ)
    if env:
        shell_env.update(env)

    # Step 1: run the configured command.
    cmd = subprocess.run(
        cfg.command,
        cwd=str(base),
        shell=True,
        env=shell_env,
        text=True,
        capture_output=True,
    )
    result.ran_command = True
    result.command_exit = cmd.returncode
    if cmd.returncode != 0:
        result.error = (
            f"command {cfg.command!r} exited {cmd.returncode}\n"
            f"stdout: {cmd.stdout}\nstderr: {cmd.stderr}"
        )
        return result

    # Step 2: check watched files for diffs.
    diffed = _has_unstaged_diff(cfg.watch, base)
    result.watched_diffed = diffed
    if not diffed:
        # Happy case: already in sync.
        return result

    # Step 3: commit + push with rebase-retry race loop.
    _git(["config", "user.name", cfg.bot_name], base)
    _git(["config", "user.email", cfg.bot_email], base)

    for path in diffed:
        _git(["add", "--", path], base)

    _git(["commit", *_build_commit_message(tag, cfg.trailers)], base)
    result.committed = True

    attempts = 0
    while True:
        attempts += 1
        result.attempts = attempts

        _git(["fetch", cfg.remote, cfg.branch], base)
        rebase = _git(["rebase", f"{cfg.remote}/{cfg.branch}"], base, check=False)
        if rebase.returncode == 0:
            push = _git(
                ["push", cfg.remote, f"HEAD:{cfg.branch}"],
                base,
                check=False,
            )
            if push.returncode == 0:
                result.pushed = True
                return result
            # Push failed — likely non-fast-forward race. Fall through
            # to rebase-abort + reset path.
        else:
            # Rebase failed — conflict (likely on a watched file from a
            # concurrent regen). Mirror pulp's pattern: abort, hard-reset
            # to the remote tip, re-run the command, and if the output
            # now differs from what's already on main, commit again.
            _git(["rebase", "--abort"], base, check=False)

        _git(["reset", "--hard", f"{cfg.remote}/{cfg.branch}"], base)

        # Re-run the command on the fresh main.
        cmd = subprocess.run(
            cfg.command,
            cwd=str(base),
            shell=True,
            env=shell_env,
            text=True,
            capture_output=True,
        )
        if cmd.returncode != 0:
            result.error = (
                f"command {cfg.command!r} exited {cmd.returncode} on retry\n"
                f"stdout: {cmd.stdout}\nstderr: {cmd.stderr}"
            )
            return result

        diffed_retry = _has_unstaged_diff(cfg.watch, base)
        if not diffed_retry:
            # Main already caught up — nothing to push.
            result.skipped_reason = "watched files already in sync after rebase"
            return result

        for path in diffed_retry:
            _git(["add", "--", path], base)
        _git(["commit", *_build_commit_message(tag, cfg.trailers)], base)

        if attempts >= cfg.max_push_attempts:
            result.error = (
                f"rebase-retry exhausted after {attempts} attempts; "
                f"main is churning faster than we can rebase"
            )
            return result

        # Backoff scaled with attempts, mirrors pulp.
        sleep(attempts * 5.0)

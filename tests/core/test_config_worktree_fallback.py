"""Git-worktree awareness in `Config.load_from_cwd`.

Covers shipyard#155: `.shipyard.local/config.toml` is gitignored so
`git worktree add` never copies it. Running `shipyard pr` from a
worktree loaded a config with `host=<no host>` for ssh targets and
preflight failed with a confusing "backend unreachable" error.

After the fix, a worktree with no `.shipyard.local/` of its own
transparently picks up the main checkout's. Main checkouts and
non-worktree layouts are unaffected.
"""

from __future__ import annotations

import subprocess
from pathlib import Path  # noqa: TC003 — used at runtime via pytest fixtures, not just annotations

import pytest

from shipyard.core.config import Config


def _git(*args: str, cwd: Path) -> None:
    subprocess.check_call(["git", *args], cwd=cwd, stdout=subprocess.DEVNULL,
                          stderr=subprocess.DEVNULL)


@pytest.fixture
def main_repo(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Seed a git repo with a tracked `.shipyard/config.toml` and a
    gitignored `.shipyard.local/config.toml`. Returns the repo root."""
    monkeypatch.setenv("GIT_AUTHOR_NAME", "T")
    monkeypatch.setenv("GIT_AUTHOR_EMAIL", "t@t")
    monkeypatch.setenv("GIT_COMMITTER_NAME", "T")
    monkeypatch.setenv("GIT_COMMITTER_EMAIL", "t@t")
    repo = tmp_path / "main"
    repo.mkdir()
    _git("init", "--quiet", "--initial-branch=main", cwd=repo)
    (repo / ".shipyard").mkdir()
    (repo / ".shipyard" / "config.toml").write_text(
        '[targets.windows]\nbackend = "ssh-windows"\n'
    )
    (repo / ".shipyard.local").mkdir()
    (repo / ".shipyard.local" / "config.toml").write_text(
        '[targets.windows]\nhost = "win.example"\n'
    )
    (repo / ".gitignore").write_text(".shipyard.local/\n")
    (repo / "README.md").write_text("seed\n")
    _git("add", ".", cwd=repo)
    _git("commit", "-q", "-m", "seed", cwd=repo)
    return repo


def test_main_checkout_uses_its_own_local(main_repo: Path) -> None:
    """No fallback path for a plain main checkout — behavior
    unchanged from pre-fix."""
    cfg = Config.load_from_cwd(main_repo)
    targets = cfg.get("targets") or {}
    assert targets.get("windows", {}).get("host") == "win.example"


def test_worktree_falls_back_to_main_checkout_local(
    main_repo: Path, tmp_path: Path
) -> None:
    """The worktree has no `.shipyard.local/` of its own. Fallback
    must find the main checkout's gitignored overlay and merge it
    into the returned config."""
    worktree = tmp_path / "feature"
    _git("worktree", "add", "-b", "feature/x", str(worktree), cwd=main_repo)

    # Sanity: the worktree has the tracked config but not the local.
    assert (worktree / ".shipyard" / "config.toml").exists()
    assert not (worktree / ".shipyard.local" / "config.toml").exists()

    cfg = Config.load_from_cwd(worktree)
    targets = cfg.get("targets") or {}
    assert targets.get("windows", {}).get("host") == "win.example", (
        "worktree should have inherited host from main checkout's "
        ".shipyard.local/config.toml"
    )


def test_worktree_with_own_local_ignores_main(
    main_repo: Path, tmp_path: Path
) -> None:
    """If a worktree has its own `.shipyard.local/`, it wins. The
    fallback only fires when the worktree-local doesn't exist."""
    worktree = tmp_path / "feature"
    _git("worktree", "add", "-b", "feature/x", str(worktree), cwd=main_repo)
    (worktree / ".shipyard.local").mkdir()
    (worktree / ".shipyard.local" / "config.toml").write_text(
        '[targets.windows]\nhost = "worktree-specific"\n'
    )

    cfg = Config.load_from_cwd(worktree)
    targets = cfg.get("targets") or {}
    assert targets.get("windows", {}).get("host") == "worktree-specific"


def test_non_git_directory_returns_none_fallback(tmp_path: Path) -> None:
    """`Config.load_from_cwd` must not crash outside a git repo. The
    fallback returns None silently in that case."""
    (tmp_path / ".shipyard").mkdir()
    (tmp_path / ".shipyard" / "config.toml").write_text(
        '[project]\nname = "test"\n'
    )
    cfg = Config.load_from_cwd(tmp_path)
    assert cfg.get("project.name") == "test"


def test_main_checkout_subdir_does_not_trigger_worktree_fallback(
    main_repo: Path,
) -> None:
    """#178 regression. Running shipyard from a subdirectory of the
    main checkout (e.g. ``repo/src/``) must *not* be mistaken for a
    linked worktree. Pre-fix, ``_worktree_main_local_dir`` compared
    ``main_checkout`` against the cwd; since ``repo/src`` != ``repo``,
    it returned ``repo/.shipyard.local`` as a fallback without
    loading ``repo/.shipyard/`` as the project overlay — a partial
    config that surfaced as wrong ssh host or missing target.

    Post-fix it compares against ``git rev-parse --show-toplevel``
    (which is ``repo`` in both main-root and subdir cases), so the
    subdir never trips the worktree branch.
    """
    from shipyard.core.config import _worktree_main_local_dir

    subdir = main_repo / "src" / "nested"
    subdir.mkdir(parents=True)

    # With the fix: subdir is identified as "inside the main checkout"
    # and no worktree fallback dir is returned.
    assert _worktree_main_local_dir(subdir) is None, (
        "subdir of the main checkout must not be treated as a "
        "linked worktree; toplevel equals main_checkout"
    )

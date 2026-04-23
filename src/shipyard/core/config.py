"""Layered TOML configuration.

Config is loaded from three layers, each overriding the previous:
  1. Machine-global:   ~/.config/shipyard/
  2. Per-project:      .shipyard/config.toml
  3. Private overlay:  .shipyard.local/config.toml

All layers are optional. The result is a merged dictionary.
"""

from __future__ import annotations

import copy
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

if sys.version_info >= (3, 11):
    import tomllib
else:
    import tomli as tomllib

import tomli_w


# Platform-appropriate global config directory
def _default_global_dir() -> Path:
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / "shipyard"
    elif sys.platform == "win32":
        return Path.home() / "AppData" / "Local" / "shipyard"
    else:
        return Path.home() / ".config" / "shipyard"


def _default_state_dir() -> Path:
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / "shipyard"
    elif sys.platform == "win32":
        return Path.home() / "AppData" / "Local" / "shipyard"
    else:
        return Path.home() / ".local" / "state" / "shipyard"


@dataclass
class Config:
    """Merged configuration from all layers."""

    data: dict[str, Any] = field(default_factory=dict)
    global_dir: Path = field(default_factory=_default_global_dir)
    project_dir: Path | None = None
    local_dir: Path | None = None

    # Convenience accessors for common config paths

    @property
    def project_name(self) -> str:
        return self.get("project.name", "unknown")

    @property
    def project_type(self) -> str | None:
        return self.get("project.type")

    @property
    def platforms(self) -> list[str]:
        return self.get("project.platforms", [])

    @property
    def targets(self) -> dict[str, Any]:
        return self.get("targets", {})

    @property
    def validation(self) -> dict[str, Any]:
        return self.get("validation", {})

    @property
    def cloud_provider(self) -> str:
        return self.get("cloud.provider", "github-hosted")

    @property
    def merge_require_platforms(self) -> list[str]:
        return self.get("merge.require_platforms", [])

    @property
    def merge_allow_mixed(self) -> bool:
        return self.get("merge.allow_mixed_evidence", True)

    @property
    def state_dir(self) -> Path:
        """Machine-global state directory for queue, logs, evidence."""
        return _default_state_dir()

    def get(self, dotted_key: str, default: Any = None) -> Any:
        """Get a value using dotted notation: 'cloud.provider'."""
        keys = dotted_key.split(".")
        node: Any = self.data
        for key in keys:
            if isinstance(node, dict) and key in node:
                node = node[key]
            else:
                return default
        return node

    def set(self, dotted_key: str, value: Any) -> None:
        """Set a value using dotted notation."""
        keys = dotted_key.split(".")
        node = self.data
        for key in keys[:-1]:
            if key not in node or not isinstance(node[key], dict):
                node[key] = {}
            node = node[key]
        node[keys[-1]] = value

    @classmethod
    def load(
        cls,
        project_dir: Path | None = None,
        local_dir: Path | None = None,
        global_dir: Path | None = None,
    ) -> Config:
        """Load and merge config from all layers.

        Later layers override earlier ones. All layers are optional.
        """
        gdir = global_dir or _default_global_dir()
        merged: dict[str, Any] = {}

        # Layer 1: machine-global
        global_config = gdir / "config.toml"
        if global_config.exists():
            _deep_merge(merged, _load_toml(global_config))

        # Layer 2: per-project
        if project_dir:
            project_config = project_dir / "config.toml"
            if project_config.exists():
                _deep_merge(merged, _load_toml(project_config))

        # Layer 3: private overlay
        if local_dir:
            local_config = local_dir / "config.toml"
            if local_config.exists():
                _deep_merge(merged, _load_toml(local_config))

        return cls(
            data=merged,
            global_dir=gdir,
            project_dir=project_dir,
            local_dir=local_dir,
        )

    @classmethod
    def load_from_cwd(cls, cwd: Path | None = None) -> Config:
        """Load config by scanning for .shipyard/ from the given directory.

        Git-worktree aware: `.shipyard.local/config.toml` is gitignored
        so `git worktree add` never copies it. The main checkout has
        the local overlay; the worktree only has the tracked
        `.shipyard/config.toml`. Without special handling, running
        `shipyard pr` from a worktree loads config that resolves
        ssh targets to `<no host>` and preflight fails with a
        confusing "backend unreachable" error even though ssh works.
        See shipyard#155.

        Fix: when the cwd is a git worktree AND it has no local
        overlay of its own, fall through to the main checkout's
        `.shipyard.local/` if one exists. Documented explicitly in
        `local_dir_source` on the returned Config so callers can
        surface "we borrowed local config from the main checkout"
        if they want.
        """
        base = cwd or Path.cwd()
        project_dir = base / ".shipyard"
        local_dir = base / ".shipyard.local"

        resolved_local_dir: Path | None = None
        if local_dir.exists() and (local_dir / "config.toml").exists():
            resolved_local_dir = local_dir
        else:
            fallback = _worktree_main_local_dir(base)
            if fallback is not None:
                resolved_local_dir = fallback

        return cls.load(
            project_dir=project_dir if project_dir.exists() else None,
            local_dir=resolved_local_dir,
        )

    def save_project(self, path: Path | None = None) -> None:
        """Write the project-layer config to disk."""
        target = path or (self.project_dir and self.project_dir / "config.toml")
        if not target:
            raise ValueError("No project directory configured")
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(tomli_w.dumps(self.data).encode())

    def to_dict(self) -> dict[str, Any]:
        return copy.deepcopy(self.data)


def _worktree_main_local_dir(base: Path) -> Path | None:
    """If `base` is inside a *linked* git worktree whose main checkout
    has a usable ``.shipyard.local/config.toml``, return that
    directory. Otherwise ``None``.

    The contract from `git worktree add`: the linked worktree's
    ``.git`` is a file pointing at ``<common>/worktrees/<name>``, and
    ``git rev-parse --git-common-dir`` resolves to the original
    checkout's ``.git`` directory. Going up one level from that gives
    the main checkout root, which is where the gitignored
    ``.shipyard.local/`` lives.

    Bail quietly (return None) on:
    - base not in a git repo at all
    - git not on PATH
    - the "common dir" path doesn't resolve to a checkout with a
      ``.shipyard.local/config.toml``
    - the base IS (or is a subdir of) the main checkout (no fallback
      needed — the main checkout's own ``.shipyard.local/`` was
      already in the regular overlay search path)

    The "subdir of the main checkout" case is #178: pre-fix, the
    comparison was against ``base`` (cwd), not the repo toplevel, so
    running shipyard from ``repo/src/`` falsely identified it as a
    linked worktree and returned ``repo/.shipyard.local`` without
    loading the matching ``repo/.shipyard/`` project overlay —
    partial config, confusing downstream failures. Now we compare
    against ``git rev-parse --show-toplevel`` so "inside the main
    checkout" consistently returns None regardless of cwd depth.
    """
    import subprocess

    def _git(args: list[str]) -> str | None:
        try:
            res = subprocess.run(
                ["git", *args],
                cwd=str(base),
                capture_output=True,
                text=True,
                timeout=3,
                check=False,
            )
        except (FileNotFoundError, subprocess.SubprocessError):
            return None
        if res.returncode != 0:
            return None
        return res.stdout.strip()

    common_dir_str = _git(["rev-parse", "--git-common-dir"])
    if common_dir_str is None:
        return None
    common_dir = Path(common_dir_str)
    if not common_dir.is_absolute():
        common_dir = (base / common_dir).resolve()
    # Main checkout root is the parent of the .git directory that
    # git-common-dir points at.
    main_checkout = common_dir.parent

    # Compare against the *current checkout's* toplevel, not against
    # cwd. In a regular repo run from a subdir, show-toplevel
    # equals main_checkout and we correctly bail. In a linked
    # worktree, show-toplevel is the worktree root and differs from
    # main_checkout, so the fallback fires.
    toplevel_str = _git(["rev-parse", "--show-toplevel"])
    if toplevel_str is None:
        return None
    try:
        if Path(toplevel_str).resolve() == main_checkout.resolve():
            return None  # we're in (or under) the main checkout
    except OSError:
        return None

    candidate = main_checkout / ".shipyard.local"
    if (candidate / "config.toml").exists():
        return candidate
    return None


def _load_toml(path: Path) -> dict[str, Any]:
    """Load a TOML file and return its contents."""
    with open(path, "rb") as f:
        return tomllib.load(f)


def _deep_merge(base: dict[str, Any], overlay: dict[str, Any]) -> None:
    """Recursively merge overlay into base. Overlay wins on conflicts."""
    for key, value in overlay.items():
        if key in base and isinstance(base[key], dict) and isinstance(value, dict):
            _deep_merge(base[key], value)
        else:
            base[key] = copy.deepcopy(value)

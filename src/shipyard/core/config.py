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
        """Load config by scanning for .shipyard/ from the given directory."""
        base = cwd or Path.cwd()
        project_dir = base / ".shipyard"
        local_dir = base / ".shipyard.local"
        return cls.load(
            project_dir=project_dir if project_dir.exists() else None,
            local_dir=local_dir if local_dir.exists() else None,
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

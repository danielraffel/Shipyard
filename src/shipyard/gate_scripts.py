"""Resolve the paths of the version-bump / skill-sync gate scripts.

Shipyard's own repo keeps these scripts under `scripts/`. Pulp — the
first external consumer — keeps them under `tools/scripts/`. Other
repos may move them elsewhere entirely. Hard-coding `scripts/` makes
`shipyard pr` unusable on any layout that isn't Shipyard's.

Resolution order (highest priority first):

  1. Environment variable override (per script).
  2. `[validation]` key in `.shipyard/config.toml` (per script).
  3. `tools/scripts/<name>` — common for repos that group CI tooling.
  4. `scripts/<name>` — Shipyard's own default.

If none of those resolves, `resolve()` raises GateScriptNotFoundError with
every path it probed plus the override knobs. Callers are expected to
print that message verbatim and exit.
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from shipyard.core.config import Config


@dataclass(frozen=True)
class GateScript:
    """One resolvable gate-script file."""

    name: str
    env_var: str
    config_key: str
    filename: str


SKILL_SYNC = GateScript(
    name="skill_sync_check",
    env_var="SHIPYARD_SKILL_SYNC_SCRIPT",
    config_key="validation.skill_sync_script",
    filename="skill_sync_check.py",
)

VERSION_BUMP = GateScript(
    name="version_bump_check",
    env_var="SHIPYARD_VERSION_BUMP_SCRIPT",
    config_key="validation.version_bump_script",
    filename="version_bump_check.py",
)

VERSIONING_CONFIG = GateScript(
    name="versioning_config",
    env_var="SHIPYARD_VERSIONING_CONFIG",
    config_key="validation.versioning_config",
    filename="versioning.json",
)

DEFAULT_DIRS: tuple[str, ...] = ("tools/scripts", "scripts")


class GateScriptNotFoundError(FileNotFoundError):
    """Raised when a gate script cannot be resolved from any source."""


def resolve(
    script: GateScript,
    repo_root: Path,
    *,
    config: Config | None = None,
    env: dict[str, str] | None = None,
) -> Path:
    """Return the resolved on-disk path of `script` or raise.

    `env` defaults to `os.environ`. `config` is optional — when
    provided, its `validation.<key>` entries are consulted. Relative
    paths in env/config are resolved against `repo_root`.
    """
    env_map = os.environ if env is None else env

    # 1. Environment variable
    env_val = env_map.get(script.env_var)
    if env_val:
        candidate = _absolute(Path(env_val), repo_root)
        if candidate.exists():
            return candidate
        raise GateScriptNotFoundError(
            _not_found_message(
                script,
                repo_root,
                probed=[(f"env {script.env_var}", candidate)],
                env_val=env_val,
            )
        )

    # 2. Config override
    config_val: str | None = None
    if config is not None:
        val = config.get(script.config_key)
        if isinstance(val, str) and val:
            config_val = val
            candidate = _absolute(Path(val), repo_root)
            if candidate.exists():
                return candidate
            raise GateScriptNotFoundError(
                _not_found_message(
                    script,
                    repo_root,
                    probed=[(f"config {script.config_key}", candidate)],
                    config_val=val,
                )
            )

    # 3 + 4. Probe default directories
    probed: list[tuple[str, Path]] = []
    for rel_dir in DEFAULT_DIRS:
        candidate = repo_root / rel_dir / script.filename
        probed.append((f"{rel_dir}/", candidate))
        if candidate.exists():
            return candidate

    raise GateScriptNotFoundError(
        _not_found_message(
            script,
            repo_root,
            probed=probed,
            env_val=None,
            config_val=config_val,
        )
    )


def _absolute(path: Path, repo_root: Path) -> Path:
    return path if path.is_absolute() else (repo_root / path).resolve()


def _not_found_message(
    script: GateScript,
    repo_root: Path,
    *,
    probed: list[tuple[str, Path]],
    env_val: str | None = None,
    config_val: str | None = None,
) -> str:
    lines = [f"shipyard: could not find {script.filename}."]
    lines.append("  Tried:")
    for label, path in probed:
        lines.append(f"    - {label} {path}")
    lines.append("")
    lines.append("  Override by setting one of:")
    lines.append(f"    - env {script.env_var}=<path>")
    lines.append(
        f"    - {script.config_key} in .shipyard/config.toml"
    )
    lines.append(
        f"    - place the file at {repo_root / 'tools' / 'scripts' / script.filename}"
    )
    lines.append(
        f"    - or at {repo_root / 'scripts' / script.filename}"
    )
    if env_val:
        lines.append("")
        lines.append(
            f"  Note: env {script.env_var}={env_val!r} did not resolve to an existing file."
        )
    if config_val and not env_val:
        lines.append("")
        lines.append(
            f"  Note: config {script.config_key}={config_val!r} did not resolve to an existing file."
        )
    return "\n".join(lines)

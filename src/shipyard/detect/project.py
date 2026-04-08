"""Project detection orchestrator.

Combines ecosystem detection, existing CI detection, and git remote
detection into a single ProjectInfo result.
"""

from __future__ import annotations

import os
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from shipyard.detect.ci_existing import CISystem, detect_existing_ci
from shipyard.detect.ecosystem import EcosystemDetector, detect_all


@dataclass(frozen=True)
class ProjectInfo:
    """Aggregated project detection results."""

    ecosystems: list[EcosystemDetector]
    existing_ci: list[CISystem]
    git_remote: str | None
    platforms: list[str]


def _get_git_remote(path: Path) -> str | None:
    """Get the origin remote URL, or None."""
    try:
        result = subprocess.run(
            ["git", "remote", "get-url", "origin"],
            capture_output=True,
            text=True,
            cwd=str(path),
            timeout=5,
        )
        if result.returncode == 0:
            return result.stdout.strip()
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass
    return None


def _infer_platforms(ecosystems: list[EcosystemDetector], path: Path) -> list[str]:
    """Infer target platforms from detected ecosystems and build files."""
    platforms: list[str] = []
    families = {e.family for e in ecosystems}
    names = {e.name for e in ecosystems}

    # Apple-only ecosystems
    if "apple" in families and families <= {"apple"}:
        return ["macos"]

    # Cross-platform by default for most ecosystems
    if "cpp" in families or "rust" in families or "go" in families:
        platforms = ["macos", "linux", "windows"]
    elif "node" in families or "python" in families:
        platforms = ["macos", "linux", "windows"]
    elif "jvm" in families:
        platforms = ["macos", "linux", "windows"]
    elif "dotnet" in families:
        platforms = ["macos", "linux", "windows"]
    elif "dart" in families:
        if "flutter" in names:
            platforms = ["macos", "linux", "windows"]
        else:
            platforms = ["macos", "linux", "windows"]
    elif "apple" in families:
        platforms = ["macos"]
    elif "ruby" in families or "elixir" in families or "php" in families:
        platforms = ["macos", "linux"]
    elif "deno" in families:
        platforms = ["macos", "linux", "windows"]
    else:
        platforms = ["macos", "linux"]

    # If there's an Apple ecosystem alongside others, ensure macos is present
    if "apple" in families and "macos" not in platforms:
        platforms.insert(0, "macos")

    return platforms


def detect_project(path: Path | str) -> ProjectInfo:
    """Detect everything about a project directory.

    Combines ecosystem detection, CI system detection, git remote
    detection, and platform inference into a single result.
    """
    path = Path(path)
    ecosystems = detect_all(path)
    existing_ci = detect_existing_ci(path)
    git_remote = _get_git_remote(path)
    platforms = _infer_platforms(ecosystems, path)

    return ProjectInfo(
        ecosystems=ecosystems,
        existing_ci=existing_ci,
        git_remote=git_remote,
        platforms=platforms,
    )

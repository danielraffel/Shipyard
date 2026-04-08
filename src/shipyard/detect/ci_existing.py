"""Existing CI configuration detection.

Scans a project directory for known CI system configurations and
returns a list of detected systems.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path


@dataclass(frozen=True)
class CISystem:
    """A detected CI system."""

    name: str
    config_path: str  # relative path from project root


# CI systems to scan for, in order of prevalence
_CI_MARKERS: list[tuple[str, str]] = [
    ("GitHub Actions", ".github/workflows"),
    ("GitLab CI", ".gitlab-ci.yml"),
    ("CircleCI", ".circleci"),
    ("Jenkins", "Jenkinsfile"),
    ("Travis CI", ".travis.yml"),
    ("Buildkite", ".buildkite"),
    ("Azure Pipelines", "azure-pipelines.yml"),
    ("Bitbucket Pipelines", "bitbucket-pipelines.yml"),
    ("Drone CI", ".drone.yml"),
    ("AppVeyor", "appveyor.yml"),
]


def detect_existing_ci(path: Path | str) -> list[CISystem]:
    """Scan for existing CI configurations in the project.

    Returns a list of CISystem entries for every CI system that has
    configuration files present.
    """
    path = Path(path)
    found: list[CISystem] = []

    for name, marker in _CI_MARKERS:
        marker_path = path / marker
        if marker_path.exists():
            found.append(CISystem(name=name, config_path=marker))

    return found

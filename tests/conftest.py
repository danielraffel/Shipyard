"""Shared test fixtures."""

from __future__ import annotations

import tempfile
from pathlib import Path

import pytest

from shipyard.core.config import Config
from shipyard.core.evidence import EvidenceStore
from shipyard.core.job import Job, Priority, ValidationMode
from shipyard.core.queue import Queue


@pytest.fixture
def tmp_dir(tmp_path: Path) -> Path:
    return tmp_path


@pytest.fixture
def queue(tmp_path: Path) -> Queue:
    return Queue(state_dir=tmp_path / "queue")


@pytest.fixture
def evidence_store(tmp_path: Path) -> EvidenceStore:
    return EvidenceStore(path=tmp_path / "evidence")


@pytest.fixture
def sample_job() -> Job:
    return Job.create(
        sha="abc1234def5678",
        branch="feature/test",
        target_names=["mac", "ubuntu", "windows"],
        mode=ValidationMode.FULL,
        priority=Priority.NORMAL,
    )


@pytest.fixture
def sample_config(tmp_path: Path) -> Config:
    """A minimal config for testing."""
    project_dir = tmp_path / ".shipyard"
    project_dir.mkdir()
    config_file = project_dir / "config.toml"
    config_file.write_text("""
[project]
name = "test-project"
type = "cmake"
platforms = ["macos", "linux", "windows"]

[validation.default]
build = "cmake --build build"
test = "ctest --test-dir build"

[targets.mac]
backend = "local"
platform = "macos-arm64"

[targets.ubuntu]
backend = "ssh"
host = "ubuntu"
platform = "linux-x64"

[targets.windows]
backend = "ssh"
host = "win"
platform = "windows-x64"
""")
    return Config.load(project_dir=project_dir)

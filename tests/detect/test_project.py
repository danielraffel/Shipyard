"""Tests for project detection orchestrator."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import patch

import pytest

from shipyard.detect.project import ProjectInfo, detect_project


class TestDetectProject:
    def test_cmake_project(self, tmp_path: Path) -> None:
        (tmp_path / "CMakeLists.txt").touch()
        with patch("shipyard.detect.project._get_git_remote", return_value=None):
            info = detect_project(tmp_path)
        assert len(info.ecosystems) == 1
        assert info.ecosystems[0].name == "cmake"
        assert "macos" in info.platforms
        assert "linux" in info.platforms
        assert "windows" in info.platforms

    def test_apple_only_project(self, tmp_path: Path) -> None:
        (tmp_path / "Package.swift").touch()
        with patch("shipyard.detect.project._get_git_remote", return_value=None):
            info = detect_project(tmp_path)
        assert info.ecosystems[0].family == "apple"
        assert info.platforms == ["macos"]

    def test_multi_ecosystem_project(self, tmp_path: Path) -> None:
        (tmp_path / "CMakeLists.txt").touch()
        (tmp_path / "package.json").touch()
        with patch("shipyard.detect.project._get_git_remote", return_value=None):
            info = detect_project(tmp_path)
        families = {e.family for e in info.ecosystems}
        assert "cpp" in families
        assert "node" in families

    def test_existing_ci_detected(self, tmp_path: Path) -> None:
        (tmp_path / ".github" / "workflows").mkdir(parents=True)
        (tmp_path / ".travis.yml").touch()
        with patch("shipyard.detect.project._get_git_remote", return_value=None):
            info = detect_project(tmp_path)
        ci_names = {c.name for c in info.existing_ci}
        assert "GitHub Actions" in ci_names
        assert "Travis CI" in ci_names

    def test_git_remote_included(self, tmp_path: Path) -> None:
        (tmp_path / "Cargo.toml").touch()
        remote = "git@github.com:user/repo.git"
        with patch("shipyard.detect.project._get_git_remote", return_value=remote):
            info = detect_project(tmp_path)
        assert info.git_remote == remote

    def test_empty_project(self, tmp_path: Path) -> None:
        with patch("shipyard.detect.project._get_git_remote", return_value=None):
            info = detect_project(tmp_path)
        assert info.ecosystems == []
        assert info.existing_ci == []
        assert info.git_remote is None

    def test_python_project_platforms(self, tmp_path: Path) -> None:
        (tmp_path / "uv.lock").touch()
        with patch("shipyard.detect.project._get_git_remote", return_value=None):
            info = detect_project(tmp_path)
        assert "macos" in info.platforms
        assert "linux" in info.platforms
        assert "windows" in info.platforms

    def test_ruby_project_platforms(self, tmp_path: Path) -> None:
        (tmp_path / "Gemfile").touch()
        with patch("shipyard.detect.project._get_git_remote", return_value=None):
            info = detect_project(tmp_path)
        # Ruby typically macos + linux, not windows
        assert "macos" in info.platforms
        assert "linux" in info.platforms


class TestProjectInfoFrozen:
    def test_projectinfo_is_frozen(self, tmp_path: Path) -> None:
        with patch("shipyard.detect.project._get_git_remote", return_value=None):
            info = detect_project(tmp_path)
        with pytest.raises(AttributeError):
            info.git_remote = "something"  # type: ignore[misc]

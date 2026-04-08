"""Tests for the init wizard."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import patch

import pytest

from shipyard.core.config import Config
from shipyard.init.wizard import _build_config_data, _ensure_gitignore, run_init
from shipyard.detect.project import ProjectInfo, detect_project


class TestRunInitNonInteractive:
    """Tests for non-interactive init flow."""

    def test_creates_config_file(self, tmp_path: Path) -> None:
        (tmp_path / "Cargo.toml").touch()
        with patch("shipyard.detect.project._get_git_remote", return_value=None):
            config = run_init(tmp_path, non_interactive=True)
        assert (tmp_path / ".shipyard" / "config.toml").exists()
        assert config.project_name == tmp_path.name

    def test_creates_gitignore_entry(self, tmp_path: Path) -> None:
        (tmp_path / "CMakeLists.txt").touch()
        with patch("shipyard.detect.project._get_git_remote", return_value=None):
            run_init(tmp_path, non_interactive=True)
        gitignore = (tmp_path / ".gitignore").read_text()
        assert ".shipyard.local/" in gitignore

    def test_preserves_existing_gitignore(self, tmp_path: Path) -> None:
        (tmp_path / ".gitignore").write_text("build/\n*.o\n")
        with patch("shipyard.detect.project._get_git_remote", return_value=None):
            run_init(tmp_path, non_interactive=True)
        gitignore = (tmp_path / ".gitignore").read_text()
        assert "build/" in gitignore
        assert ".shipyard.local/" in gitignore

    def test_no_duplicate_gitignore_entry(self, tmp_path: Path) -> None:
        (tmp_path / ".gitignore").write_text(".shipyard.local/\n")
        with patch("shipyard.detect.project._get_git_remote", return_value=None):
            run_init(tmp_path, non_interactive=True)
        gitignore = (tmp_path / ".gitignore").read_text()
        assert gitignore.count(".shipyard.local/") == 1

    def test_config_has_ecosystem_type(self, tmp_path: Path) -> None:
        (tmp_path / "Cargo.toml").touch()
        with patch("shipyard.detect.project._get_git_remote", return_value=None):
            config = run_init(tmp_path, non_interactive=True)
        assert config.project_type == "rust"

    def test_config_has_platforms(self, tmp_path: Path) -> None:
        (tmp_path / "go.mod").touch()
        with patch("shipyard.detect.project._get_git_remote", return_value=None):
            config = run_init(tmp_path, non_interactive=True)
        platforms = config.platforms
        assert "macos" in platforms
        assert "linux" in platforms

    def test_config_has_validation_commands(self, tmp_path: Path) -> None:
        (tmp_path / "Cargo.toml").touch()
        with patch("shipyard.detect.project._get_git_remote", return_value=None):
            config = run_init(tmp_path, non_interactive=True)
        validation = config.validation
        assert "default" in validation
        assert "cargo" in validation["default"].get("build", "")


class TestEnsureGitignore:
    def test_creates_gitignore_if_missing(self, tmp_path: Path) -> None:
        _ensure_gitignore(tmp_path)
        assert (tmp_path / ".gitignore").exists()
        assert ".shipyard.local/" in (tmp_path / ".gitignore").read_text()

    def test_appends_to_existing(self, tmp_path: Path) -> None:
        (tmp_path / ".gitignore").write_text("node_modules/\n")
        _ensure_gitignore(tmp_path)
        content = (tmp_path / ".gitignore").read_text()
        assert "node_modules/" in content
        assert ".shipyard.local/" in content

    def test_handles_no_trailing_newline(self, tmp_path: Path) -> None:
        (tmp_path / ".gitignore").write_text("build/")
        _ensure_gitignore(tmp_path)
        content = (tmp_path / ".gitignore").read_text()
        # Should have added newline before the entry
        assert "build/\n.shipyard.local/\n" in content


class TestBuildConfigData:
    def test_basic_structure(self, tmp_path: Path) -> None:
        (tmp_path / "CMakeLists.txt").touch()
        with patch("shipyard.detect.project._get_git_remote", return_value=None):
            info = detect_project(tmp_path)
        data = _build_config_data(
            info,
            project_name="my-project",
            platforms=["macos", "linux"],
            cloud_provider="github-hosted",
            ssh_hosts={},
        )
        assert data["project"]["name"] == "my-project"
        assert data["project"]["type"] == "cmake"
        assert "macos" in data["project"]["platforms"]

    def test_ssh_hosts_added(self, tmp_path: Path) -> None:
        (tmp_path / "Cargo.toml").touch()
        with patch("shipyard.detect.project._get_git_remote", return_value=None):
            info = detect_project(tmp_path)
        data = _build_config_data(
            info,
            project_name="proj",
            platforms=["macos", "linux", "windows"],
            cloud_provider="namespace",
            ssh_hosts={"ubuntu": "my-ubuntu", "windows": "my-win"},
        )
        assert data["targets"]["ubuntu"]["backend"] == "ssh"
        assert data["targets"]["ubuntu"]["host"] == "my-ubuntu"
        assert data["targets"]["windows"]["backend"] == "ssh"
        assert data["targets"]["windows"]["host"] == "my-win"
        assert data["cloud"]["provider"] == "namespace"

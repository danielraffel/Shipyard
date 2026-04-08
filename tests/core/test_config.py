"""Tests for config loading and layer merging."""

from __future__ import annotations

from pathlib import Path

from shipyard.core.config import Config


class TestConfig:
    def test_load_project_config(self, sample_config: Config) -> None:
        assert sample_config.project_name == "test-project"
        assert sample_config.project_type == "cmake"
        assert "macos" in sample_config.platforms

    def test_dotted_get(self, sample_config: Config) -> None:
        assert sample_config.get("project.name") == "test-project"
        assert sample_config.get("targets.mac.backend") == "local"
        assert sample_config.get("nonexistent.key", "default") == "default"

    def test_dotted_set(self, sample_config: Config) -> None:
        sample_config.set("cloud.provider", "namespace")
        assert sample_config.get("cloud.provider") == "namespace"

    def test_set_creates_intermediate_dicts(self, sample_config: Config) -> None:
        sample_config.set("new.deeply.nested.key", "value")
        assert sample_config.get("new.deeply.nested.key") == "value"

    def test_targets_accessor(self, sample_config: Config) -> None:
        targets = sample_config.targets
        assert "mac" in targets
        assert "ubuntu" in targets
        assert "windows" in targets
        assert targets["mac"]["backend"] == "local"

    def test_layer_merging(self, tmp_path: Path) -> None:
        # Global layer
        global_dir = tmp_path / "global"
        global_dir.mkdir()
        (global_dir / "config.toml").write_text("""
[cloud]
provider = "github-hosted"

[defaults]
priority = "normal"
""")

        # Project layer (overrides cloud.provider)
        project_dir = tmp_path / ".shipyard"
        project_dir.mkdir()
        (project_dir / "config.toml").write_text("""
[cloud]
provider = "namespace"

[project]
name = "my-project"
""")

        config = Config.load(
            global_dir=global_dir,
            project_dir=project_dir,
        )

        # Project layer wins on conflict
        assert config.get("cloud.provider") == "namespace"
        # Global layer preserved where no conflict
        assert config.get("defaults.priority") == "normal"
        # Project-only values present
        assert config.get("project.name") == "my-project"

    def test_three_layer_merge(self, tmp_path: Path) -> None:
        global_dir = tmp_path / "global"
        global_dir.mkdir()
        (global_dir / "config.toml").write_text('[defaults]\npriority = "normal"\n')

        project_dir = tmp_path / ".shipyard"
        project_dir.mkdir()
        (project_dir / "config.toml").write_text('[project]\nname = "proj"\n')

        local_dir = tmp_path / ".shipyard.local"
        local_dir.mkdir()
        (local_dir / "config.toml").write_text('[targets.ubuntu]\nhost = "my-vm.local"\n')

        config = Config.load(
            global_dir=global_dir,
            project_dir=project_dir,
            local_dir=local_dir,
        )

        assert config.get("defaults.priority") == "normal"
        assert config.get("project.name") == "proj"
        assert config.get("targets.ubuntu.host") == "my-vm.local"

    def test_empty_config(self, tmp_path: Path) -> None:
        config = Config.load(
            global_dir=tmp_path / "nonexistent",
            project_dir=tmp_path / "also-nonexistent",
        )
        assert config.data == {}
        assert config.project_name == "unknown"

    def test_save_project(self, tmp_path: Path) -> None:
        project_dir = tmp_path / ".shipyard"
        project_dir.mkdir()
        config = Config(data={"project": {"name": "saved"}}, project_dir=project_dir)
        config.save_project()
        assert (project_dir / "config.toml").exists()

        reloaded = Config.load(project_dir=project_dir)
        assert reloaded.get("project.name") == "saved"

    def test_to_dict_is_deep_copy(self, sample_config: Config) -> None:
        d = sample_config.to_dict()
        d["project"]["name"] = "mutated"
        assert sample_config.project_name == "test-project"  # unchanged

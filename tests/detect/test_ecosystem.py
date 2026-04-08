"""Tests for ecosystem detection."""

from __future__ import annotations

from pathlib import Path

import pytest

from shipyard.detect.ecosystem import (
    EcosystemDetector,
    detect,
    detect_all,
    detect_package_manager,
)


class TestDetectSingle:
    """Tests for detect() — returns first matching ecosystem."""

    def test_cmake_project(self, tmp_path: Path) -> None:
        (tmp_path / "CMakeLists.txt").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "cmake"
        assert result.family == "cpp"

    def test_rust_project(self, tmp_path: Path) -> None:
        (tmp_path / "Cargo.toml").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "rust"

    def test_go_project(self, tmp_path: Path) -> None:
        (tmp_path / "go.mod").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "go"

    def test_swift_spm_project(self, tmp_path: Path) -> None:
        (tmp_path / "Package.swift").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "swift-spm"
        assert result.family == "apple"

    def test_xcode_project(self, tmp_path: Path) -> None:
        (tmp_path / "MyApp.xcodeproj").mkdir()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "xcode"
        assert result.family == "apple"

    def test_empty_directory(self, tmp_path: Path) -> None:
        result = detect(tmp_path)
        assert result is None

    def test_ruby_project(self, tmp_path: Path) -> None:
        (tmp_path / "Gemfile").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "ruby"

    def test_elixir_project(self, tmp_path: Path) -> None:
        (tmp_path / "mix.exs").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "elixir"

    def test_php_project(self, tmp_path: Path) -> None:
        (tmp_path / "composer.json").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "php"

    def test_deno_project(self, tmp_path: Path) -> None:
        (tmp_path / "deno.json").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "deno"

    def test_deno_jsonc(self, tmp_path: Path) -> None:
        (tmp_path / "deno.jsonc").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "deno"


class TestNodePriority:
    """Tests for Node.js package manager priority ordering."""

    def test_pnpm_over_npm(self, tmp_path: Path) -> None:
        (tmp_path / "package.json").touch()
        (tmp_path / "pnpm-lock.yaml").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "node-pnpm"

    def test_yarn_over_npm(self, tmp_path: Path) -> None:
        (tmp_path / "package.json").touch()
        (tmp_path / "yarn.lock").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "node-yarn"

    def test_bun_over_yarn(self, tmp_path: Path) -> None:
        (tmp_path / "package.json").touch()
        (tmp_path / "bun.lockb").touch()
        (tmp_path / "yarn.lock").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "node-bun"

    def test_npm_lockfile_over_default(self, tmp_path: Path) -> None:
        (tmp_path / "package.json").touch()
        (tmp_path / "package-lock.json").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "node-npm"

    def test_package_json_only(self, tmp_path: Path) -> None:
        (tmp_path / "package.json").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "node-npm-default"


class TestPythonPriority:
    """Tests for Python package manager priority ordering."""

    def test_uv_over_poetry(self, tmp_path: Path) -> None:
        (tmp_path / "uv.lock").touch()
        (tmp_path / "poetry.lock").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "python-uv"

    def test_poetry_over_pip(self, tmp_path: Path) -> None:
        (tmp_path / "poetry.lock").touch()
        (tmp_path / "requirements.txt").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "python-poetry"

    def test_pipenv(self, tmp_path: Path) -> None:
        (tmp_path / "Pipfile.lock").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "python-pipenv"

    def test_requirements_txt(self, tmp_path: Path) -> None:
        (tmp_path / "requirements.txt").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "python-pip"

    def test_setup_py(self, tmp_path: Path) -> None:
        (tmp_path / "setup.py").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "python-setuptools"


class TestDetectAll:
    """Tests for detect_all() — returns all ecosystems with family dedup."""

    def test_multi_ecosystem(self, tmp_path: Path) -> None:
        (tmp_path / "CMakeLists.txt").touch()
        (tmp_path / "package.json").touch()
        (tmp_path / "pnpm-lock.yaml").touch()
        results = detect_all(tmp_path)
        names = {r.name for r in results}
        assert "cmake" in names
        assert "node-pnpm" in names
        # Should not include node-npm-default (family dedup)
        assert "node-npm-default" not in names

    def test_family_dedup_keeps_highest_priority(self, tmp_path: Path) -> None:
        (tmp_path / "package.json").touch()
        (tmp_path / "yarn.lock").touch()
        results = detect_all(tmp_path)
        node_results = [r for r in results if r.family == "node"]
        assert len(node_results) == 1
        assert node_results[0].name == "node-yarn"

    def test_empty_returns_empty(self, tmp_path: Path) -> None:
        results = detect_all(tmp_path)
        assert results == []

    def test_multiple_families(self, tmp_path: Path) -> None:
        (tmp_path / "Cargo.toml").touch()
        (tmp_path / "Package.swift").touch()
        (tmp_path / "requirements.txt").touch()
        results = detect_all(tmp_path)
        families = {r.family for r in results}
        assert families == {"rust", "apple", "python"}


class TestFlutterDart:
    """Tests for Flutter vs Dart detection."""

    def test_flutter_detected(self, tmp_path: Path) -> None:
        (tmp_path / "pubspec.yaml").write_text(
            "dependencies:\n  flutter:\n    sdk: flutter\n"
        )
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "flutter"

    def test_dart_without_flutter(self, tmp_path: Path) -> None:
        (tmp_path / "pubspec.yaml").write_text(
            "name: my_app\ndependencies:\n  http: ^1.0.0\n"
        )
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "dart"


class TestGlobDetectors:
    """Tests for glob-based detection (Xcode, .NET)."""

    def test_xcworkspace(self, tmp_path: Path) -> None:
        (tmp_path / "MyApp.xcworkspace").mkdir()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "xcode"

    def test_dotnet_csproj(self, tmp_path: Path) -> None:
        (tmp_path / "MyApp.csproj").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "dotnet"

    def test_dotnet_sln(self, tmp_path: Path) -> None:
        (tmp_path / "MyApp.sln").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "dotnet"


class TestJVM:
    """Tests for Gradle and Maven detection."""

    def test_gradle(self, tmp_path: Path) -> None:
        (tmp_path / "build.gradle").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "gradle"

    def test_gradle_kts(self, tmp_path: Path) -> None:
        (tmp_path / "build.gradle.kts").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "gradle"

    def test_maven(self, tmp_path: Path) -> None:
        (tmp_path / "pom.xml").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.name == "maven"

    def test_gradle_over_maven(self, tmp_path: Path) -> None:
        (tmp_path / "build.gradle").touch()
        (tmp_path / "pom.xml").touch()
        results = detect_all(tmp_path)
        jvm_results = [r for r in results if r.family == "jvm"]
        assert len(jvm_results) == 1
        assert jvm_results[0].name == "gradle"


class TestDetectPackageManager:
    """Tests for Node.js package manager detection."""

    def test_pnpm(self, tmp_path: Path) -> None:
        (tmp_path / "pnpm-lock.yaml").touch()
        assert detect_package_manager(tmp_path) == "pnpm"

    def test_bun(self, tmp_path: Path) -> None:
        (tmp_path / "bun.lockb").touch()
        assert detect_package_manager(tmp_path) == "bun"

    def test_yarn(self, tmp_path: Path) -> None:
        (tmp_path / "yarn.lock").touch()
        assert detect_package_manager(tmp_path) == "yarn"

    def test_npm(self, tmp_path: Path) -> None:
        (tmp_path / "package-lock.json").touch()
        assert detect_package_manager(tmp_path) == "npm"

    def test_none(self, tmp_path: Path) -> None:
        assert detect_package_manager(tmp_path) is None


class TestValidationCommands:
    """Tests that detected ecosystems have sensible commands."""

    def test_cmake_has_build_and_test(self, tmp_path: Path) -> None:
        (tmp_path / "CMakeLists.txt").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.commands.build is not None
        assert result.commands.test is not None
        assert "cmake" in result.commands.build

    def test_rust_has_build_and_test(self, tmp_path: Path) -> None:
        (tmp_path / "Cargo.toml").touch()
        result = detect(tmp_path)
        assert result is not None
        assert result.commands.build is not None
        assert "cargo" in result.commands.build

"""Ecosystem detection registry.

Detects project ecosystems by scanning for marker files (lockfiles,
build manifests, config files). Supports priority-ordered detection
within families (e.g. pnpm > yarn > npm for Node.js).
"""

from __future__ import annotations

import glob
import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable


@dataclass(frozen=True)
class ValidationCommands:
    """Commands for each validation stage."""

    install: str | None = None
    build: str | None = None
    test: str | None = None
    validate: str | None = None


@dataclass(frozen=True)
class EcosystemDetector:
    """A single ecosystem detection entry."""

    name: str
    family: str
    markers: list[str]
    commands: ValidationCommands
    check_function: Callable[[Path], bool] | None = None
    priority: int = 0  # higher = checked first within family


def _has_glob_match(path: Path, pattern: str) -> bool:
    """Check if any file matching a glob pattern exists under path."""
    return bool(glob.glob(str(path / pattern)))


def _check_xcode_project(path: Path) -> bool:
    """Check for *.xcodeproj or *.xcworkspace."""
    return _has_glob_match(path, "*.xcodeproj") or _has_glob_match(path, "*.xcworkspace")


def _check_dotnet(path: Path) -> bool:
    """Check for *.csproj, *.fsproj, or *.sln."""
    return (
        _has_glob_match(path, "*.csproj")
        or _has_glob_match(path, "*.fsproj")
        or _has_glob_match(path, "*.sln")
    )


def _check_flutter(path: Path) -> bool:
    """Check for pubspec.yaml with flutter in dependencies."""
    pubspec = path / "pubspec.yaml"
    if not pubspec.exists():
        return False
    try:
        content = pubspec.read_text(encoding="utf-8")
        return "flutter:" in content or "flutter_test:" in content
    except OSError:
        return False


def _check_dart(path: Path) -> bool:
    """Check for pubspec.yaml without flutter."""
    pubspec = path / "pubspec.yaml"
    if not pubspec.exists():
        return False
    try:
        content = pubspec.read_text(encoding="utf-8")
        return "flutter:" not in content
    except OSError:
        return False


ECOSYSTEM_REGISTRY: list[EcosystemDetector] = [
    # ---- C/C++ ----
    EcosystemDetector(
        name="cmake",
        family="cpp",
        markers=["CMakeLists.txt"],
        commands=ValidationCommands(
            build="cmake -S . -B build && cmake --build build",
            test="ctest --test-dir build --output-on-failure",
        ),
    ),

    # ---- Apple ----
    EcosystemDetector(
        name="swift-spm",
        family="apple",
        markers=["Package.swift"],
        commands=ValidationCommands(
            build="swift build",
            test="swift test",
        ),
        priority=10,
    ),
    EcosystemDetector(
        name="xcode",
        family="apple",
        markers=[],  # uses check_function instead
        commands=ValidationCommands(
            build="xcodebuild -scheme default build",
            test="xcodebuild -scheme default test",
        ),
        check_function=_check_xcode_project,
        priority=5,
    ),

    # ---- Rust ----
    EcosystemDetector(
        name="rust",
        family="rust",
        markers=["Cargo.toml"],
        commands=ValidationCommands(
            build="cargo build",
            test="cargo test",
        ),
    ),

    # ---- Go ----
    EcosystemDetector(
        name="go",
        family="go",
        markers=["go.mod"],
        commands=ValidationCommands(
            build="go build ./...",
            test="go test ./...",
        ),
    ),

    # ---- Node.js (ordered by priority: pnpm > bun > yarn > npm) ----
    EcosystemDetector(
        name="node-pnpm",
        family="node",
        markers=["pnpm-lock.yaml"],
        commands=ValidationCommands(
            install="pnpm install --frozen-lockfile",
            build="pnpm run build",
            test="pnpm test",
        ),
        priority=50,
    ),
    EcosystemDetector(
        name="node-bun",
        family="node",
        markers=["bun.lockb"],
        commands=ValidationCommands(
            install="bun install --frozen-lockfile",
            build="bun run build",
            test="bun test",
        ),
        priority=40,
    ),
    EcosystemDetector(
        name="node-yarn",
        family="node",
        markers=["yarn.lock"],
        commands=ValidationCommands(
            install="yarn install --frozen-lockfile",
            build="yarn build",
            test="yarn test",
        ),
        priority=30,
    ),
    EcosystemDetector(
        name="node-npm",
        family="node",
        markers=["package-lock.json"],
        commands=ValidationCommands(
            install="npm ci",
            build="npm run build",
            test="npm test",
        ),
        priority=20,
    ),
    EcosystemDetector(
        name="node-npm-default",
        family="node",
        markers=["package.json"],
        commands=ValidationCommands(
            install="npm install",
            build="npm run build",
            test="npm test",
        ),
        priority=10,
    ),

    # ---- Python (ordered by priority: uv > poetry > pipenv > pip > setup.py) ----
    EcosystemDetector(
        name="python-uv",
        family="python",
        markers=["uv.lock"],
        commands=ValidationCommands(
            install="uv sync",
            build="uv run python -m build",
            test="uv run pytest",
        ),
        priority=50,
    ),
    EcosystemDetector(
        name="python-poetry",
        family="python",
        markers=["poetry.lock"],
        commands=ValidationCommands(
            install="poetry install",
            build="poetry build",
            test="poetry run pytest",
        ),
        priority=40,
    ),
    EcosystemDetector(
        name="python-pipenv",
        family="python",
        markers=["Pipfile.lock"],
        commands=ValidationCommands(
            install="pipenv install",
            test="pipenv run pytest",
        ),
        priority=30,
    ),
    EcosystemDetector(
        name="python-pip",
        family="python",
        markers=["requirements.txt"],
        commands=ValidationCommands(
            install="pip install -r requirements.txt",
            test="pytest",
        ),
        priority=20,
    ),
    EcosystemDetector(
        name="python-setuptools",
        family="python",
        markers=["setup.py"],
        commands=ValidationCommands(
            install="pip install -e .",
            build="python setup.py build",
            test="pytest",
        ),
        priority=10,
    ),

    # ---- JVM ----
    EcosystemDetector(
        name="gradle",
        family="jvm",
        markers=["build.gradle", "build.gradle.kts"],
        commands=ValidationCommands(
            build="./gradlew build",
            test="./gradlew test",
        ),
        priority=10,
    ),
    EcosystemDetector(
        name="maven",
        family="jvm",
        markers=["pom.xml"],
        commands=ValidationCommands(
            build="mvn package",
            test="mvn test",
        ),
        priority=5,
    ),

    # ---- .NET ----
    EcosystemDetector(
        name="dotnet",
        family="dotnet",
        markers=[],  # uses check_function
        commands=ValidationCommands(
            install="dotnet restore",
            build="dotnet build",
            test="dotnet test",
        ),
        check_function=_check_dotnet,
    ),

    # ---- Flutter / Dart ----
    EcosystemDetector(
        name="flutter",
        family="dart",
        markers=[],  # uses check_function (pubspec.yaml with flutter)
        commands=ValidationCommands(
            install="flutter pub get",
            build="flutter build",
            test="flutter test",
        ),
        check_function=_check_flutter,
        priority=10,
    ),
    EcosystemDetector(
        name="dart",
        family="dart",
        markers=["pubspec.yaml"],
        commands=ValidationCommands(
            install="dart pub get",
            test="dart test",
        ),
        check_function=_check_dart,
        priority=5,
    ),

    # ---- Deno ----
    EcosystemDetector(
        name="deno",
        family="deno",
        markers=["deno.json", "deno.jsonc"],
        commands=ValidationCommands(
            test="deno test",
        ),
    ),

    # ---- Ruby ----
    EcosystemDetector(
        name="ruby",
        family="ruby",
        markers=["Gemfile"],
        commands=ValidationCommands(
            install="bundle install",
            test="bundle exec rake test",
        ),
    ),

    # ---- Elixir ----
    EcosystemDetector(
        name="elixir",
        family="elixir",
        markers=["mix.exs"],
        commands=ValidationCommands(
            install="mix deps.get",
            build="mix compile",
            test="mix test",
        ),
    ),

    # ---- PHP ----
    EcosystemDetector(
        name="php",
        family="php",
        markers=["composer.json"],
        commands=ValidationCommands(
            install="composer install",
            test="./vendor/bin/phpunit",
        ),
    ),
]


def _matches(detector: EcosystemDetector, path: Path) -> bool:
    """Check if a detector matches the given project path."""
    # check_function takes precedence when markers list is empty
    if detector.check_function is not None:
        return detector.check_function(path)
    return any(os.path.exists(path / marker) for marker in detector.markers)


def detect(path: Path | str) -> EcosystemDetector | None:
    """Detect the primary ecosystem for a project.

    Returns the first (highest-priority) matching detector, or None.
    Detectors are sorted by priority descending before checking.
    """
    path = Path(path)
    sorted_registry = sorted(ECOSYSTEM_REGISTRY, key=lambda d: d.priority, reverse=True)
    for detector in sorted_registry:
        if _matches(detector, path):
            return detector
    return None


def detect_all(path: Path | str) -> list[EcosystemDetector]:
    """Detect all matching ecosystems with family deduplication.

    Within each family, only the highest-priority match is returned.
    Results are ordered by priority descending.
    """
    path = Path(path)
    sorted_registry = sorted(ECOSYSTEM_REGISTRY, key=lambda d: d.priority, reverse=True)
    seen_families: set[str] = set()
    results: list[EcosystemDetector] = []

    for detector in sorted_registry:
        if detector.family in seen_families:
            continue
        if _matches(detector, path):
            seen_families.add(detector.family)
            results.append(detector)

    return results


def detect_package_manager(path: Path | str) -> str | None:
    """Detect the Node.js package manager for a project.

    Returns the package manager name (pnpm, bun, yarn, npm) based on
    lockfile presence, or None if no Node.js project is detected.
    """
    path = Path(path)
    lockfile_map = [
        ("pnpm-lock.yaml", "pnpm"),
        ("bun.lockb", "bun"),
        ("yarn.lock", "yarn"),
        ("package-lock.json", "npm"),
        ("package.json", "npm"),
    ]
    for filename, manager in lockfile_map:
        if os.path.exists(path / filename):
            return manager
    return None

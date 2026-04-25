"""Pytest fixtures for the Shipyard sandbox E2E harness.

Binary discovery (in priority order):

1. Env-var override:
     SHIPYARD_BINARY_FOR_TEST  — explicit path to a shipyard binary

2. PyInstaller release artifacts inside the repo:
     dist/shipyard
     build/dist/shipyard
     pyinstaller/dist/shipyard

3. Installed-binary fallback:
     ~/.local/bin/shipyard   (the canonical install location)

4. Final fallback: an in-repo ``python -m shipyard.cli`` wrapper
   (Sandbox.stage_python_wrapper). This always works as long as the
   Shipyard package source tree lives under ``src/shipyard/``.

Tests that require a binary call the ``shipyard_binary`` fixture; the
``sandbox_with_shipyard`` fixture stages either the discovered binary
or — when none is available — the python wrapper. Either way the test
gets a sandbox with a working ``shipyard`` on its shadowed PATH.
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Iterator

import pytest

from shipyard_sandbox import Sandbox


# ----- repo-root discovery ---------------------------------------------------


def _find_repo_root() -> Path:
    """Walk upward from this file until we find ``.git`` (a dir for a
    normal clone, a file for a worktree)."""
    current = Path(__file__).resolve().parent
    for candidate in (current, *current.parents):
        if (candidate / ".git").exists():
            return candidate
    return Path(__file__).resolve().parents[2]


REPO_ROOT = _find_repo_root()


# ----- binary discovery ------------------------------------------------------


def _discover_binary() -> Path | None:
    """Locate a real ``shipyard`` binary to stage. Returns ``None`` when
    only the python-wrapper fallback is viable.

    Discovery order trades fidelity for speed: a venv entrypoint script
    or a fresh dist/ build runs in ~0.5s; the user's installed
    PyInstaller --onefile binary takes ~7s per invocation (the onefile
    extractor is the cost). Both exercise the same ``shipyard.cli:main``
    Click entrypoint, so the fast lane is correct for PR-gating
    purposes. Set ``SHIPYARD_BINARY_FOR_TEST`` to force a specific
    binary (e.g. the release artifact)."""
    override = os.environ.get("SHIPYARD_BINARY_FOR_TEST")
    if override:
        candidate = Path(override).expanduser()
        return candidate if candidate.exists() else None
    candidates: list[Path] = []
    # Fast lane: a venv we've installed `shipyard` into. This matches
    # how `pip install -e .` lands the entrypoint.
    venv_candidates = [
        REPO_ROOT / ".venv" / "bin" / "shipyard",
        REPO_ROOT / ".venv-test" / "bin" / "shipyard",
        REPO_ROOT / "venv" / "bin" / "shipyard",
    ]
    candidates.extend(venv_candidates)
    # PyInstaller release artifacts under the repo (these are fresh
    # builds — slow but representative).
    candidates.extend([
        REPO_ROOT / "dist" / "shipyard",
        REPO_ROOT / "build" / "dist" / "shipyard",
        REPO_ROOT / "pyinstaller" / "dist" / "shipyard",
    ])
    # The user's installed binary is the slowest option (PyInstaller
    # --onefile cold-start is ~7s on macOS) but it's the one users
    # actually invoke. Keep it last so we prefer faster paths when
    # available.
    candidates.append(Path.home() / ".local" / "bin" / "shipyard")
    for c in candidates:
        if c.exists() and c.is_file():
            return c
    return None


# ----- fixtures --------------------------------------------------------------


@pytest.fixture(scope="session")
def repo_root() -> Path:
    return REPO_ROOT


@pytest.fixture(scope="session")
def shipyard_binary() -> Path | None:
    """The discovered shipyard binary, or ``None`` if only the
    python-wrapper fallback is available. Tests that strictly require
    a packaged binary should ``pytest.skip`` themselves on ``None``."""
    return _discover_binary()


@pytest.fixture(scope="session")
def surface_roots(repo_root: Path) -> list[Path]:
    """Filesystem roots the surface enumerator should scan for
    ``shipyard X`` invocations. Order doesn't matter — the result set
    is unioned."""
    return [
        repo_root / "commands",
        repo_root / "README.md",
        repo_root / "docs",
        repo_root / ".github" / "workflows",
        repo_root / "skills",
    ]


@pytest.fixture()
def sandbox() -> Iterator[Sandbox]:
    """Per-test sandbox. Teardown runs the contamination audit — any
    write to the user's real Shipyard install fails the test here."""
    sbx = Sandbox()
    sbx.setup()
    try:
        yield sbx
        sbx.assert_no_contamination()
    finally:
        sbx.teardown()


@pytest.fixture()
def sandbox_with_shipyard(
    sandbox: Sandbox,
    shipyard_binary: Path | None,
    repo_root: Path,
) -> Sandbox:
    """Per-test sandbox with ``shipyard`` already staged. Uses the
    discovered binary if available, falls back to a python wrapper
    that runs ``python -m shipyard.cli`` against the in-repo source."""
    if shipyard_binary is not None:
        sandbox.stage_binary(shipyard_binary, as_name="shipyard")
    else:
        sandbox.stage_python_wrapper(repo_root, as_name="shipyard")
    return sandbox


# ----- reporting -------------------------------------------------------------


def pytest_report_header(config: pytest.Config) -> list[str]:
    bin_path = _discover_binary()
    return [
        f"shipyard sandbox e2e: binary    = "
        f"{bin_path or '(none — using python-wrapper fallback)'}",
        f"shipyard sandbox e2e: repo root = {REPO_ROOT}",
    ]


def pytest_configure(config: pytest.Config) -> None:  # noqa: D401
    """Make ``shipyard_sandbox`` importable regardless of where pytest
    is invoked from."""
    import sys as _sys

    harness_dir = str(Path(__file__).resolve().parent)
    if harness_dir not in _sys.path:
        _sys.path.insert(0, harness_dir)

#!/usr/bin/env python3
"""Regression tests for Shipyard's release-surface path coverage."""

from __future__ import annotations

import json
from pathlib import Path
import unittest

import version_bump_check


ROOT = Path(__file__).resolve().parents[1]


class VersionSurfaceConfigTests(unittest.TestCase):
    def test_cli_surface_covers_root_and_nested_rust_sources(self) -> None:
        config = json.loads((ROOT / "scripts/versioning.json").read_text())
        patterns = config["surfaces"]["cli"]["trigger_paths"]

        self.assertTrue(version_bump_check._matches_any("src/ship.rs", patterns))
        self.assertTrue(
            version_bump_check._matches_any("src/backend/local.rs", patterns)
        )

    def test_version_surface_config_covers_itself(self) -> None:
        config = json.loads((ROOT / "scripts/versioning.json").read_text())
        patterns = config["surfaces"]["cli"]["trigger_paths"]

        self.assertTrue(
            version_bump_check._matches_any("scripts/versioning.json", patterns)
        )


if __name__ == "__main__":
    unittest.main()

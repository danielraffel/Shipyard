#!/usr/bin/env python3
"""Focused acceptance tests for version-at-land and the PR gate."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent


def run(cwd: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(args, cwd=cwd, text=True, capture_output=True, check=check)


class VersionAtLandTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = Path(self.tmp.name)
        run(self.repo, "git", "init", "-q", "-b", "main")
        run(self.repo, "git", "config", "user.email", "test@example.com")
        run(self.repo, "git", "config", "user.name", "Test")
        (self.repo / "scripts").mkdir()
        for name in ("version_bump_check.py", "version_at_land.py", "versioning.json"):
            shutil.copy(HERE / name, self.repo / "scripts" / name)
        (self.repo / "Cargo.toml").write_text('[package]\nname = "shipyard"\nversion = "0.292.0"\n')
        (self.repo / "Cargo.lock").write_text('version = 3\n\n[[package]]\nname = "shipyard"\nversion = "0.292.0"\n')
        (self.repo / ".claude-plugin").mkdir()
        (self.repo / ".claude-plugin/plugin.json").write_text('{"version":"0.111.0"}\n')
        (self.repo / ".claude-plugin/marketplace.json").write_text('{"plugins":[{"version":"0.111.0"}]}\n')
        run(self.repo, "git", "add", ".")
        run(self.repo, "git", "commit", "-qm", "base")
        self.base = run(self.repo, "git", "rev-parse", "HEAD").stdout.strip()

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def gate(self) -> subprocess.CompletedProcess[str]:
        return run(self.repo, "python3", "scripts/version_bump_check.py", "--base", self.base,
                   "--config", "scripts/versioning.json", "--mode=report", check=False)

    def test_pr_without_bump_passes(self) -> None:
        (self.repo / "src").mkdir()
        (self.repo / "src/change.rs").write_text("pub fn change() {}\n")
        run(self.repo, "git", "add", "src/change.rs")
        run(self.repo, "git", "commit", "-qm", "fix: keep release counter out of PR")
        result = self.gate()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("assigned post-merge", result.stdout)

    def test_pr_with_version_edit_is_refused(self) -> None:
        path = self.repo / "Cargo.toml"
        path.write_text(path.read_text().replace("0.292.0", "0.292.1"))
        run(self.repo, "git", "add", "Cargo.toml")
        run(self.repo, "git", "commit", "-qm", "chore: bump version")
        result = self.gate()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("owned by version-at-land", result.stdout)

    def test_release_exception_is_explicit(self) -> None:
        path = self.repo / "Cargo.toml"
        path.write_text(path.read_text().replace("0.292.0", "0.292.1"))
        run(self.repo, "git", "add", "Cargo.toml")
        run(self.repo, "git", "commit", "-qm", "release: recover\n\nRelease: allow-version-files reason=\"recovery\"")
        result = self.gate()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_two_landed_changes_coalesce_without_version_conflict(self) -> None:
        (self.repo / "src").mkdir()
        (self.repo / "src/cli.rs").write_text("pub fn cli() {}\n")
        run(self.repo, "git", "add", "src/cli.rs")
        run(self.repo, "git", "commit", "-qm", "fix: cli")
        (self.repo / "commands").mkdir()
        (self.repo / "commands/new.md").write_text("new command\n")
        run(self.repo, "git", "add", "commands/new.md")
        run(self.repo, "git", "commit", "-qm", "feat: plugin")
        result = run(self.repo, "python3", "scripts/version_at_land.py", "--base", self.base,
                     "--head", "HEAD", "--config", "scripts/versioning.json", "--json")
        payload = json.loads(result.stdout)
        self.assertEqual(payload["status"], "dry-run")
        self.assertEqual({item["surface"] for item in payload["plan"]}, {"cli", "plugin"})
        self.assertEqual({item["assigned"] for item in payload["plan"]}, {"0.292.1", "0.112.0"})


if __name__ == "__main__":
    unittest.main()

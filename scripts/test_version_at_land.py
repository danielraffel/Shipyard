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

    def test_cargo_lock_edit_is_refused(self) -> None:
        path = self.repo / "Cargo.lock"
        path.write_text(path.read_text().replace("0.292.0", "0.292.1"))
        run(self.repo, "git", "add", "Cargo.lock")
        run(self.repo, "git", "commit", "-qm", "chore: edit generated lock")
        result = self.gate()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Cargo.lock", result.stdout)

    def test_release_exception_is_explicit(self) -> None:
        path = self.repo / "Cargo.toml"
        path.write_text(path.read_text().replace("0.292.0", "0.292.1"))
        run(self.repo, "git", "add", "Cargo.toml")
        run(self.repo, "git", "commit", "-qm", "release: recover\n\nRelease: allow-version-files reason=\"recovery\"")
        result = self.gate()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_empty_release_reason_does_not_bypass_gate(self) -> None:
        path = self.repo / "Cargo.toml"
        path.write_text(path.read_text().replace("0.292.0", "0.292.1"))
        run(self.repo, "git", "add", "Cargo.toml")
        run(self.repo, "git", "commit", "-qm", "release: malformed\n\nRelease: allow-version-files reason=\"\"")
        result = self.gate()
        self.assertNotEqual(result.returncode, 0)

    def test_level_none_change_is_a_noop(self) -> None:
        (self.repo / "commands").mkdir()
        (self.repo / "commands/whitespace.md").write_text("old\n")
        run(self.repo, "git", "add", "commands/whitespace.md")
        run(self.repo, "git", "commit", "-qm", "base plugin file")
        base2 = run(self.repo, "git", "rev-parse", "HEAD").stdout.strip()
        (self.repo / "commands/whitespace.md").write_text("old   \n")
        run(self.repo, "git", "add", "commands/whitespace.md")
        run(self.repo, "git", "commit", "-qm", "docs: whitespace only")
        result = run(self.repo, "python3", "scripts/version_at_land.py", "--base", base2,
                     "--head", "HEAD", "--config", "scripts/versioning.json", "--json")
        self.assertEqual(json.loads(result.stdout)["plan"], [])

    def test_marker_drain_starts_after_applied_writer_commit(self) -> None:
        (self.repo / "src").mkdir()
        (self.repo / "src/first.rs").write_text("pub fn first() {}\n")
        run(self.repo, "git", "add", "src/first.rs")
        run(self.repo, "git", "commit", "-qm", "fix: first")
        first = run(self.repo, "git", "rev-parse", "HEAD").stdout.strip()
        cargo = self.repo / "Cargo.toml"
        cargo.write_text(cargo.read_text().replace("0.292.0", "0.292.1"))
        lock = self.repo / "Cargo.lock"
        lock.write_text(lock.read_text().replace("0.292.0", "0.292.1"))
        run(self.repo, "git", "add", "Cargo.toml", "Cargo.lock")
        run(self.repo, "git", "commit", "-qm", f"chore: assign versions at land\n\nVersion-Bump-Applied: {first}")
        (self.repo / "src/second.rs").write_text("pub fn second() {}\n")
        run(self.repo, "git", "add", "src/second.rs")
        run(self.repo, "git", "commit", "-qm", "fix: second")
        result = run(self.repo, "python3", "scripts/version_at_land.py", "--base", self.base,
                     "--head", "HEAD", "--config", "scripts/versioning.json", "--json")
        payload = json.loads(result.stdout)
        self.assertEqual(payload["plan"][0]["current"], "0.292.1")
        self.assertEqual(payload["plan"][0]["assigned"], "0.292.2")

    def test_release_workflows_fail_closed_without_bot_token(self) -> None:
        for name in ("version-at-land.yml", "auto-release.yml"):
            workflow = (HERE.parent / ".github/workflows" / name).read_text()
            self.assertNotIn("|| secrets.GITHUB_TOKEN", workflow)
            self.assertIn("RELEASE_BOT_TOKEN is required", workflow)

    def test_gate_has_no_apply_mode(self) -> None:
        result = run(self.repo, "python3", "scripts/version_bump_check.py",
                     "--mode=apply", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("invalid choice", result.stderr)

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

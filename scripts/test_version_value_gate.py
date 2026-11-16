#!/usr/bin/env python3
"""The PR gate refuses version changes, not every edit to a version file."""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import version_bump_check  # noqa: E402

CARGO = """[package]
name = "shipyard"
version = "0.292.0"
edition = "2021"

[features]
ci-test-home = []
experimental-authority-v5 = []

[dependencies]
serde = "1.0"
"""

LOCK = """version = 3

[[package]]
name = "serde"
version = "1.0.200"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "shipyard"
version = "0.292.0"
dependencies = [
 "serde",
]
"""

PLUGIN = '{\n  "name": "shipyard",\n  "description": "CI",\n  "version": "0.111.0"\n}\n'
MARKET = ('{\n  "metadata": {"version": "0.1.8"},\n'
          '  "plugins": [{"name": "shipyard", "description": "CI", "version": "0.111.0"}]\n}\n')


def run(cwd: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(args, cwd=cwd, text=True, capture_output=True, check=check)


class VersionValueGateTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = Path(self.tmp.name)
        run(self.repo, "git", "init", "-q", "-b", "main")
        run(self.repo, "git", "config", "user.email", "test@example.com")
        run(self.repo, "git", "config", "user.name", "Test")
        (self.repo / "scripts").mkdir()
        for name in ("version_bump_check.py", "versioning.json"):
            shutil.copy(HERE / name, self.repo / "scripts" / name)
        (self.repo / "Cargo.toml").write_text(CARGO)
        (self.repo / "Cargo.lock").write_text(LOCK)
        (self.repo / ".claude-plugin").mkdir()
        (self.repo / ".claude-plugin/plugin.json").write_text(PLUGIN)
        (self.repo / ".claude-plugin/marketplace.json").write_text(MARKET)
        run(self.repo, "git", "add", ".")
        run(self.repo, "git", "commit", "-qm", "base")
        self.base = run(self.repo, "git", "rev-parse", "HEAD").stdout.strip()

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def edit(self, path: str, old: str, new: str) -> None:
        target = self.repo / path
        text = target.read_text()
        self.assertIn(old, text)
        target.write_text(text.replace(old, new, 1))

    def commit(self, message: str = "chore: edit") -> None:
        run(self.repo, "git", "add", "-A")
        run(self.repo, "git", "commit", "-qm", message)

    def gate(self) -> subprocess.CompletedProcess[str]:
        return run(self.repo, "python3", "scripts/version_bump_check.py", "--base", self.base,
                   "--config", "scripts/versioning.json", "--mode=report", check=False)

    def assert_passes(self) -> None:
        result = self.gate()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("owned by version-at-land", result.stdout)

    def assert_refused(self, *paths: str) -> None:
        result = self.gate()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("owned by version-at-land", result.stdout)
        refused = next(line for line in result.stdout.splitlines()
                       if "owned by version-at-land" in line)
        for path in paths:
            self.assertIn(path, refused)

    def value_edits(self) -> list[str]:
        """version_value_edits in process, for files the rest of the gate
        (read_version in assess_surfaces) cannot read and exits on."""
        cfg = version_bump_check.load_config(self.repo / "scripts/versioning.json")
        cwd = os.getcwd()
        try:
            os.chdir(self.repo)
            return version_bump_check.version_value_edits(cfg, self.base, "HEAD")
        finally:
            os.chdir(cwd)

    # Cargo.toml and Cargo.lock

    def test_updater_incompatible_cargo_quoting_is_refused(self) -> None:
        for path, old, new in (
            ("Cargo.toml", 'name = "shipyard"', "name = 'shipyard'"),
            ("Cargo.lock", 'name = "shipyard"', "name = 'shipyard'"),
            ("Cargo.lock", 'version = "0.292.0"', "version = '0.292.0'"),
        ):
            with self.subTest(path=path, new=new):
                original = (self.repo / path).read_text()
                self.edit(path, old, new)
                self.commit()
                try:
                    self.assert_refused(path)
                finally:
                    run(self.repo, "git", "reset", "--hard", "-q", self.base)
                self.assertEqual((self.repo / path).read_text(), original)

    def test_dependency_only_edit_passes(self) -> None:
        self.edit("Cargo.toml", 'serde = "1.0"\n', 'serde = "1.0"\nanyhow = "1"\n')
        self.edit("Cargo.lock", "[[package]]\nname = \"shipyard\"",
                  '[[package]]\nname = "anyhow"\nversion = "1.0.86"\n'
                  'source = "registry+https://github.com/rust-lang/crates.io-index"\n\n'
                  '[[package]]\nname = "shipyard"')
        self.edit("Cargo.lock", ' "serde",\n', ' "anyhow",\n "serde",\n')
        self.commit("feat: add anyhow")
        self.assert_passes()

    def test_features_edit_passes(self) -> None:
        self.edit("Cargo.toml", "experimental-authority-v5 = []\n", "")
        self.commit("chore: remove a feature")
        self.assert_passes()

    def test_package_version_change_fails(self) -> None:
        self.edit("Cargo.toml", 'version = "0.292.0"', 'version = "0.292.1"')
        self.commit()
        self.assert_refused("Cargo.toml")

    def test_version_change_hidden_among_other_edits_fails(self) -> None:
        self.edit("Cargo.toml", "experimental-authority-v5 = []\n", "")
        self.edit("Cargo.toml", 'serde = "1.0"\n', 'serde = "1.0"\nanyhow = "1"\n')
        self.edit("Cargo.toml", 'version = "0.292.0"', 'version = "0.293.0"')
        self.commit("chore: tidy features")
        self.assert_refused("Cargo.toml")

    def test_version_change_in_a_later_commit_of_the_range_fails(self) -> None:
        self.edit("Cargo.toml", "experimental-authority-v5 = []\n", "")
        self.commit("chore: remove a feature")
        self.edit("Cargo.toml", 'version = "0.292.0"', 'version = "0.292.1"')
        self.commit("chore: second commit")
        self.assert_refused("Cargo.toml")

    def test_unparseable_cargo_toml_fails(self) -> None:
        self.edit("Cargo.toml", "[features]\n", "[features\n")
        self.commit()
        self.assert_refused("Cargo.toml")

    def test_workspace_inherited_version_fails(self) -> None:
        self.edit("Cargo.toml", 'version = "0.292.0"', "version.workspace = true")
        self.commit()
        self.assert_refused("Cargo.toml")

    def test_decoy_version_the_regex_would_read_fails(self) -> None:
        # The configured regex skips a single-quoted version and runs on to
        # the decoy, which still reads 0.292.0. Cargo builds 9.9.9.
        self.edit("Cargo.toml", 'version = "0.292.0"', "version = '9.9.9'")
        self.edit("Cargo.toml", "[features]\n",
                  '[package.metadata.decoy]\nversion = "0.292.0"\n\n[features]\n')
        self.commit()
        self.assert_refused("Cargo.toml")

    def test_version_the_release_tagger_would_read_fails(self) -> None:
        # Cargo and the configured reader both still read 0.292.0, but
        # auto-release.yml's reader allows leading whitespace and would tag
        # 9.9.9 from the indented line inside the multi-line string.
        self.edit("Cargo.toml", 'name = "shipyard"\n',
                  'name = "shipyard"\ndescription = """\n  version = "9.9.9"\n"""\n')
        self.commit()
        self.assert_refused("Cargo.toml")

    def test_package_table_edit_fails(self) -> None:
        self.edit("Cargo.toml", 'name = "shipyard"\n',
                  'name = "shipyard"\ndescription = """\nCross-platform CI.\n"""\n')
        self.commit()
        self.assert_refused("Cargo.toml")

    def test_build_script_in_package_fails(self) -> None:
        self.edit("Cargo.toml", 'edition = "2021"\n', 'edition = "2021"\nbuild = "build.rs"\n')
        self.commit()
        self.assert_refused("Cargo.toml")

    def test_build_target_and_workspace_tables_fail(self) -> None:
        for name, addition in (
            ("workspace", '\n[workspace]\nmembers = ["x"]\n'),
            ("lib", '\n[lib]\npath = "src/other.rs"\n'),
            ("bin", '\n[[bin]]\nname = "planted"\npath = "src/planted.rs"\n'),
            ("example", '\n[[example]]\nname = "planted"\npath = "src/planted.rs"\n'),
            ("test", '\n[[test]]\nname = "planted"\npath = "src/planted.rs"\n'),
            ("bench", '\n[[bench]]\nname = "planted"\npath = "src/planted.rs"\n'),
        ):
            with self.subTest(table=name):
                run(self.repo, "git", "reset", "-q", "--hard", self.base)
                with (self.repo / "Cargo.toml").open("a") as manifest:
                    manifest.write(addition)
                self.commit()
                self.assert_refused("Cargo.toml")

    def test_top_level_target_discovery_keys_fail(self) -> None:
        for key in ("autobins", "autoexamples", "autotests", "autobenches"):
            with self.subTest(key=key):
                run(self.repo, "git", "reset", "-q", "--hard", self.base)
                text = (self.repo / "Cargo.toml").read_text()
                (self.repo / "Cargo.toml").write_text(f"{key} = false\n" + text)
                self.commit()
                self.assert_refused("Cargo.toml")

    def test_profile_and_lint_edits_pass(self) -> None:
        with (self.repo / "Cargo.toml").open("a") as manifest:
            manifest.write('\n[profile.release]\nlto = true\n\n[lints.clippy]\npedantic = "warn"\n')
        self.commit("chore: tune profile")
        self.assert_passes()

    def test_downstream_reader_matches_the_release_tagger(self) -> None:
        workflow = (HERE.parent / ".github/workflows/auto-release.yml").read_text()
        self.assertIn(r'^\[package\].*?^\s*version\s*=\s*\"([^\"]+)\"', workflow)
        self.assertEqual(version_bump_check._CARGO_DOWNSTREAM_VERSION_RES[0].pattern,
                         r'(?ms)^\[package\].*?^\s*version\s*=\s*"([^"]+)"')

    def test_non_utf8_cargo_toml_fails(self) -> None:
        (self.repo / "Cargo.toml").write_bytes(CARGO.encode() + b"# \xff\n")
        self.commit()
        self.assertEqual(self.value_edits(), ["Cargo.toml"])
        # read_version in assess_surfaces exits on it before the report.
        self.assertNotEqual(self.gate().returncode, 0)

    def test_without_tomllib_a_cargo_edit_keeps_the_whole_file_rule(self) -> None:
        self.edit("Cargo.toml", "experimental-authority-v5 = []\n", "")
        self.commit("chore: remove a feature")
        saved = version_bump_check._tomllib
        try:
            self.assertEqual(self.value_edits(), [])
            version_bump_check._tomllib = None
            self.assertEqual(self.value_edits(), ["Cargo.toml"])
        finally:
            version_bump_check._tomllib = saved

    def test_crate_rename_fails(self) -> None:
        self.edit("Cargo.toml", 'name = "shipyard"', 'name = "shipyard2"')
        self.commit()
        self.assert_refused("Cargo.toml")

    def test_cargo_lock_own_version_change_fails(self) -> None:
        self.edit("Cargo.lock", 'name = "shipyard"\nversion = "0.292.0"',
                  'name = "shipyard"\nversion = "0.292.1"')
        self.commit()
        self.assert_refused("Cargo.lock")

    def test_cargo_lock_dependency_version_change_passes(self) -> None:
        self.edit("Cargo.lock", 'version = "1.0.200"', 'version = "1.0.201"')
        self.commit("chore: cargo update serde")
        self.assert_passes()

    def test_cargo_lock_second_own_entry_fails(self) -> None:
        with (self.repo / "Cargo.lock").open("a") as lock:
            lock.write('\n[[package]]\nname = "shipyard"\nversion = "0.292.0"\n')
        self.commit()
        self.assert_refused("Cargo.lock")

    def test_cargo_lock_unparseable_fails(self) -> None:
        self.edit("Cargo.lock", "[[package]]\nname = \"serde\"", "[[package]\nname = \"serde\"")
        self.commit()
        self.assert_refused("Cargo.lock")

    def test_deleted_version_file_fails(self) -> None:
        run(self.repo, "git", "rm", "-q", "Cargo.lock")
        self.commit()
        self.assert_refused("Cargo.lock")

    def test_added_version_file_fails(self) -> None:
        run(self.repo, "git", "rm", "-q", ".claude-plugin/marketplace.json")
        self.commit()
        self.base = run(self.repo, "git", "rev-parse", "HEAD").stdout.strip()
        (self.repo / ".claude-plugin/marketplace.json").write_text(MARKET)
        self.commit()
        self.assert_refused(".claude-plugin/marketplace.json")

    def test_renamed_away_version_file_fails(self) -> None:
        run(self.repo, "git", "mv", ".claude-plugin/plugin.json", ".claude-plugin/plugin2.json")
        self.commit()
        self.assert_refused(".claude-plugin/plugin.json")

    def test_head_is_read_from_git_not_the_worktree(self) -> None:
        self.edit("Cargo.toml", 'version = "0.292.0"', 'version = "0.292.1"')
        self.commit()
        # The worktree carries the old version again; the commit still moves it.
        self.edit("Cargo.toml", 'version = "0.292.1"', 'version = "0.292.0"')
        self.assert_refused("Cargo.toml")

    # Symlinks and non-file entries

    def swap_for_symlink(self, path: str) -> None:
        """Replace `path` with a symlink whose target string is the old
        contents, so its blob id is unchanged and only the mode moves."""
        oid = run(self.repo, "git", "rev-parse", f"HEAD:{path}").stdout.strip()
        run(self.repo, "git", "update-index", "--cacheinfo", f"120000,{oid},{path}")
        run(self.repo, "git", "commit", "-qm", "chore: swap")
        self.assertEqual(run(self.repo, "git", "rev-parse", f"HEAD:{path}").stdout.strip(), oid)

    def test_symlink_swap_fails_for_every_version_file(self) -> None:
        for path in ("Cargo.toml", "Cargo.lock", ".claude-plugin/plugin.json",
                     ".claude-plugin/marketplace.json"):
            with self.subTest(path=path):
                run(self.repo, "git", "reset", "-q", "--hard", self.base)
                self.swap_for_symlink(path)
                self.assertEqual(self.value_edits(), [path])
                self.assert_refused(path)

    def test_tree_at_version_file_path_fails(self) -> None:
        run(self.repo, "git", "rm", "-q", "Cargo.lock")
        (self.repo / "Cargo.lock").mkdir()
        (self.repo / "Cargo.lock/planted").write_text(LOCK)
        self.commit()
        self.assert_refused("Cargo.lock")

    def test_executable_bit_flip_fails(self) -> None:
        run(self.repo, "git", "update-index", "--chmod=+x", "Cargo.toml")
        run(self.repo, "git", "commit", "-qm", "chore: chmod")
        self.assert_refused("Cargo.toml")

    # Plugin manifests keep the whole-file rule

    def test_plugin_json_non_version_edit_fails(self) -> None:
        self.edit(".claude-plugin/plugin.json", '"description": "CI"', '"description": "CI agent"')
        self.commit()
        self.assert_refused(".claude-plugin/plugin.json")

    def test_plugin_json_version_edit_fails(self) -> None:
        self.edit(".claude-plugin/plugin.json", '"0.111.0"', '"0.112.0"')
        self.commit()
        self.assert_refused(".claude-plugin/plugin.json")

    def test_plugin_json_min_shipyard_version_edit_fails(self) -> None:
        self.edit(".claude-plugin/plugin.json", '"version": "0.111.0"',
                  '"version": "0.111.0",\n  "min_shipyard_version": "0.500.0"')
        self.commit()
        self.assert_refused(".claude-plugin/plugin.json")

    def test_marketplace_non_version_edit_fails(self) -> None:
        self.edit(".claude-plugin/marketplace.json", '"description": "CI"',
                  '"description": "CI agent"')
        self.commit()
        self.assert_refused(".claude-plugin/marketplace.json")

    # Release bypass

    def test_release_trailer_still_admits_a_version_change(self) -> None:
        self.edit("Cargo.toml", 'version = "0.292.0"', 'version = "0.292.1"')
        self.edit("Cargo.lock", 'name = "shipyard"\nversion = "0.292.0"',
                  'name = "shipyard"\nversion = "0.292.1"')
        self.edit(".claude-plugin/plugin.json", '"0.111.0"', '"0.112.0"')
        self.edit(".claude-plugin/marketplace.json", '"0.111.0"', '"0.112.0"')
        self.commit('chore: assign versions at land\n\n'
                    'Release: allow-version-files reason="version-at-land bump pull request"')
        self.assert_passes()

    def test_release_trailer_without_a_reason_still_refuses(self) -> None:
        self.edit("Cargo.toml", 'version = "0.292.0"', 'version = "0.292.1"')
        self.commit('chore: bump\n\nRelease: allow-version-files reason=""')
        self.assert_refused("Cargo.toml")

    def test_untouched_version_files_pass(self) -> None:
        (self.repo / "README.md").write_text("docs\n")
        self.commit("docs: readme")
        self.assert_passes()


if __name__ == "__main__":
    unittest.main()

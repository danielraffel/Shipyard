#!/usr/bin/env python3
"""Tests for Cargo.lock refresh on a version bump.

Shipyard validates itself with `cargo test --all-targets --locked`, so a bump
that edits Cargo.toml and leaves Cargo.lock at the old version fails its own
validation before any code is even compiled.

Run:
    python3 scripts/test_version_bump_cargo_lock.py
"""

from __future__ import annotations

import importlib.util
import pathlib
import re
import sys
import tempfile
import unittest


REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
_spec = importlib.util.spec_from_file_location(
    "version_bump_check", REPO_ROOT / "scripts" / "version_bump_check.py"
)
assert _spec and _spec.loader
vbc = importlib.util.module_from_spec(_spec)
# Register before exec: @dataclass resolves annotations via
# sys.modules[cls.__module__], which raises on 3.9 if the module is absent.
sys.modules[_spec.name] = vbc
_spec.loader.exec_module(vbc)

_PACKAGE_VERSION_RE = re.compile(r'(?ms)^\[package\].*?^version\s*=\s*"([^"]+)"')


MANIFEST = """\
[package]
name = "shipyard"
version = "0.80.1"
edition = "2024"
rust-version = "1.92"

[dependencies]
serde = "1"
"""

# A workspace member (no `source`) plus a registry dependency, so the
# member-vs-dependency distinction is actually exercised.
LOCK = """\
version = 4

[[package]]
name = "serde"
version = "1.0.228"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "shipyard"
version = "0.80.1"
dependencies = [
 "serde",
]
"""


def _repo(manifest: str = MANIFEST, lock: str | None = LOCK) -> pathlib.Path:
    root = pathlib.Path(tempfile.mkdtemp())
    (root / "Cargo.toml").write_text(manifest)
    if lock is not None:
        (root / "Cargo.lock").write_text(lock)
    return root


class CargoCrateName(unittest.TestCase):
    def test_reads_the_package_name(self):
        self.assertEqual(vbc.cargo_crate_name(MANIFEST), "shipyard")

    def test_ignores_a_name_from_another_table(self):
        # `[dependencies]`-style tables also carry `name` keys in some manifests;
        # only the `[package]` name identifies the crate.
        manifest = '[dependencies]\nname = "not-the-crate"\n'
        self.assertIsNone(vbc.cargo_crate_name(manifest))

    def test_returns_none_without_a_package_table(self):
        self.assertIsNone(vbc.cargo_crate_name("[workspace]\nmembers = []\n"))


class RefreshCargoLock(unittest.TestCase):
    def test_bumps_the_workspace_member_entry(self):
        root = _repo()
        result = vbc.refresh_cargo_lock(root, "Cargo.toml", "0.80.2")
        self.assertEqual(result, "Cargo.lock")
        text = (root / "Cargo.lock").read_text()
        self.assertIn('name = "shipyard"\nversion = "0.80.2"', text)

    def test_leaves_registry_dependencies_untouched(self):
        root = _repo()
        vbc.refresh_cargo_lock(root, "Cargo.toml", "0.80.2")
        text = (root / "Cargo.lock").read_text()
        # The dependency keeps its own version and its source line.
        self.assertIn('name = "serde"\nversion = "1.0.228"', text)
        self.assertIn(
            "source = \"registry+https://github.com/rust-lang/crates.io-index\"",
            text,
        )

    def test_preserves_everything_else_byte_for_byte(self):
        root = _repo()
        vbc.refresh_cargo_lock(root, "Cargo.toml", "0.80.2")
        text = (root / "Cargo.lock").read_text()
        self.assertEqual(text, LOCK.replace('version = "0.80.1"', 'version = "0.80.2"'))

    def test_no_lockfile_is_not_an_error(self):
        root = _repo(lock=None)
        self.assertIsNone(vbc.refresh_cargo_lock(root, "Cargo.toml", "0.80.2"))

    def test_crate_absent_from_lock_leaves_it_alone(self):
        # Fail loud at build time rather than guess which entry to rewrite.
        lock = LOCK.replace('name = "shipyard"', 'name = "something-else"')
        root = _repo(lock=lock)
        self.assertIsNone(vbc.refresh_cargo_lock(root, "Cargo.toml", "0.80.2"))
        self.assertEqual((root / "Cargo.lock").read_text(), lock)

    def test_ambiguous_duplicate_member_entries_leave_it_alone(self):
        lock = LOCK + '\n[[package]]\nname = "shipyard"\nversion = "0.1.0"\n'
        root = _repo(lock=lock)
        self.assertIsNone(vbc.refresh_cargo_lock(root, "Cargo.toml", "0.80.2"))
        self.assertEqual((root / "Cargo.lock").read_text(), lock)

    def test_a_same_named_registry_crate_is_not_mistaken_for_the_member(self):
        # Only the entry without `source` is the workspace member.
        lock = """\
version = 4

[[package]]
name = "shipyard"
version = "0.2.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "shipyard"
version = "0.80.1"
dependencies = []
"""
        root = _repo(lock=lock)
        self.assertEqual(
            vbc.refresh_cargo_lock(root, "Cargo.toml", "0.80.2"), "Cargo.lock"
        )
        text = (root / "Cargo.lock").read_text()
        self.assertIn('version = "0.2.0"', text)  # registry entry untouched
        self.assertIn('version = "0.80.2"', text)  # member bumped
        self.assertNotIn('version = "0.80.1"', text)

    def test_is_idempotent(self):
        root = _repo()
        vbc.refresh_cargo_lock(root, "Cargo.toml", "0.80.2")
        first = (root / "Cargo.lock").read_text()
        vbc.refresh_cargo_lock(root, "Cargo.toml", "0.80.2")
        self.assertEqual((root / "Cargo.lock").read_text(), first)


class RealRepo(unittest.TestCase):
    """The actual Shipyard manifest/lock pair this bug was found on."""

    def test_this_repo_lock_and_manifest_agree(self):
        manifest = REPO_ROOT / "Cargo.toml"
        lock = REPO_ROOT / "Cargo.lock"
        if not (manifest.exists() and lock.exists()):
            self.skipTest("not a cargo checkout")
        manifest_text = manifest.read_text()
        crate = vbc.cargo_crate_name(manifest_text)
        self.assertIsNotNone(crate)
        declared = _PACKAGE_VERSION_RE.search(manifest_text)
        self.assertIsNotNone(declared, "Cargo.toml declares no [package] version")
        locked = re.search(
            rf'(?ms)\[\[package\]\]\nname = "{re.escape(crate)}"\nversion = "([^"]+)"',
            lock.read_text(),
        )
        self.assertIsNotNone(locked, "lock has no workspace-member entry for the crate")
        # If these disagree, `cargo --locked` is already broken on this checkout —
        # which is the exact breakage this module exists to prevent.
        self.assertEqual(locked.group(1), declared.group(1))


if __name__ == "__main__":
    unittest.main()

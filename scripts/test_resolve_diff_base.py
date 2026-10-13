#!/usr/bin/env python3
"""The versioning gates' base for a run that is not a pull_request event.

Reproduces the false failure seen on danielraffel/Shipyard#795 and #799:
a branch cut before a version-at-land commit, checked by a dispatched run
against `origin/main`, reads main's version bump as its own edit.
"""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import resolve_diff_base

SCRIPTS = Path(__file__).resolve().parent
GIT_ENV = {
    "GIT_AUTHOR_NAME": "t",
    "GIT_AUTHOR_EMAIL": "t@example.com",
    "GIT_COMMITTER_NAME": "t",
    "GIT_COMMITTER_EMAIL": "t@example.com",
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_NOSYSTEM": "1",
}


def git(cwd: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args],
        cwd=cwd,
        check=True,
        capture_output=True,
        text=True,
        env={**os.environ, **GIT_ENV},
    ).stdout.strip()


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


class DispatchedRunBaseTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="resolve-diff-base-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        upstream = self.tmp / "upstream"
        upstream.mkdir()
        git(upstream, "init", "-q", "-b", "main")
        write(upstream / "Cargo.toml", '[package]\nname = "shipyard"\nversion = "0.1.0"\n')
        write(upstream / "scripts" / "versioning.json",
              (SCRIPTS / "versioning.json").read_text(encoding="utf-8"))
        git(upstream, "add", ".")
        git(upstream, "commit", "-q", "-m", "base")
        self.cut = git(upstream, "rev-parse", "HEAD")
        self.work = self.tmp / "work"
        git(self.tmp, "clone", "-q", str(upstream), str(self.work))
        git(self.work, "checkout", "-q", "-b", "feat/docs")
        write(self.work / "docs" / "note.md", "a docs-only change\n")
        git(self.work, "add", ".")
        git(self.work, "commit", "-q", "-m", "docs: a note")
        # Main lands a version after the branch was cut.
        write(upstream / "Cargo.toml", '[package]\nname = "shipyard"\nversion = "0.2.0"\n')
        git(upstream, "commit", "-q", "-am", "chore: assign versions at land")
        git(self.work, "fetch", "-q", "origin")

    def version_check(self, base: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(SCRIPTS / "version_bump_check.py"), "--base", base,
             "--config", "scripts/versioning.json", "--mode=report"],
            cwd=self.work,
            capture_output=True,
            text=True,
            env={**os.environ, **GIT_ENV},
        )

    def test_a_dispatched_run_diffs_from_where_the_branch_left_main(self) -> None:
        base = resolve_diff_base.resolve("workflow_dispatch", "", "main", cwd=str(self.work))
        self.assertEqual(base, self.cut)
        result = self.version_check(base)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("owned by version-at-land", result.stdout + result.stderr)

    def test_origin_main_reads_mains_version_bump_as_the_branchs(self) -> None:
        # The control: the base the workflow used before reports the false
        # failure on this very branch.
        result = self.version_check("origin/main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("owned by version-at-land", result.stdout + result.stderr)
        self.assertIn("Cargo.toml", result.stdout + result.stderr)

    def test_a_pull_request_run_keeps_its_base_branch(self) -> None:
        self.assertEqual(
            resolve_diff_base.resolve("pull_request", "main", "main", cwd=str(self.work)),
            "origin/main",
        )
        self.assertEqual(
            resolve_diff_base.resolve("pull_request", "release", "main", cwd=str(self.work)),
            "origin/release",
        )

    def test_a_missing_base_branch_is_an_error_not_a_guess(self) -> None:
        with self.assertRaises(SystemExit):
            resolve_diff_base.resolve("workflow_dispatch", "", "no-such-branch",
                                      cwd=str(self.work))


if __name__ == "__main__":
    unittest.main()

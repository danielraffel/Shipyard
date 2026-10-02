#!/usr/bin/env python3
"""Tests for scripts/version_ahead_of_main.py."""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from io import StringIO
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import version_ahead_of_main as gate  # noqa: E402


def cargo(version: str) -> str:
    return f'[package]\nname = "shipyard"\nversion = "{version}"\n\n[dependencies]\nfoo = {{ version = "9.9.9" }}\n'


class VerdictTests(unittest.TestCase):
    def test_reads_the_package_version_not_a_dependency(self) -> None:
        self.assertEqual(gate.cargo_version(cargo("0.245.0")), "0.245.0")

    def test_only_a_strictly_higher_version_passes(self) -> None:
        self.assertTrue(gate.verdict("0.246.0", "0.245.0")[0])
        self.assertFalse(gate.verdict("0.245.0", "0.245.0")[0])
        self.assertFalse(gate.verdict("0.244.9", "0.245.0")[0])
        self.assertFalse(gate.verdict(None, "0.245.0")[0])

    def test_two_prs_at_the_same_version_the_second_fails_once_the_first_lands(self) -> None:
        # Both PRs were cut from main at 0.244.0 and bumped to 0.245.0. The
        # first merged; the sweep run on that push must turn the second red.
        heads = {"aaa": cargo("0.245.0"), "bbb": cargo("0.246.0")}
        posted: list[tuple[str, str]] = []
        results = gate.sweep(
            "0.245.0",
            [
                {"number": 680, "head": {"sha": "aaa"}},
                {"number": 681, "head": {"sha": "bbb"}},
            ],
            heads.get,
            lambda sha, state, _description: posted.append((sha, state)),
        )
        self.assertEqual(posted, [("aaa", "failure"), ("bbb", "success")])
        self.assertEqual([(number, ok) for number, ok, _ in results], [(680, False), (681, True)])

    def test_an_unreadable_head_is_posted_as_a_failure(self) -> None:
        posted: list[str] = []
        gate.sweep(
            "0.245.0",
            [{"number": 1, "head": {"sha": "ccc"}}],
            lambda _sha: None,
            lambda _sha, state, _description: posted.append(state),
        )
        self.assertEqual(posted, ["failure"])


class CheckCommandTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = Path(self.tmp.name)
        env = {**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t",
               "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t"}
        self.git = lambda *args: subprocess.run(
            ["git", *args], cwd=self.repo, env=env, check=True, capture_output=True
        )
        self.git("init", "-q", "-b", "main")
        self.commit("0.244.0")

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def commit(self, version: str) -> None:
        (self.repo / "Cargo.toml").write_text(cargo(version), encoding="utf-8")
        self.git("add", "Cargo.toml")
        self.git("commit", "-q", "-m", version)

    def run_check(self) -> int:
        cwd = os.getcwd()
        os.chdir(self.repo)
        try:
            with redirect_stdout(StringIO()), redirect_stderr(StringIO()):
                return gate.main(["check", "--head-ref", "pr", "--main-ref", "main"])
        finally:
            os.chdir(cwd)

    def test_the_second_pr_at_the_same_version_fails_against_live_main(self) -> None:
        self.git("checkout", "-q", "-b", "pr")
        self.commit("0.245.0")
        self.git("checkout", "-q", "main")
        self.assertEqual(self.run_check(), 0, "control: ahead of main passes")
        self.commit("0.245.0")  # the other PR landed first
        self.assertEqual(self.run_check(), 1)


if __name__ == "__main__":
    unittest.main()

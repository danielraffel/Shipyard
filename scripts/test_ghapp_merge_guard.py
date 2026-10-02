#!/usr/bin/env python3
"""Hermetic tests for scripts/ghapp_merge_guard.sh, the `merge-guard` ghapp runs.

The guard refuses `pr merge` on a repository that cannot enforce required
checks server-side (a private repo on a free plan) until the configured checks
are green, and refuses `--auto` there outright. Its `gh` and its config are
both injectable, so these tests drive it with a fake check reporter.
"""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
GUARD = HERE / "ghapp_merge_guard.sh"
WRAPPER = HERE / "ghapp"
CONFIG = {
    "Generous-Corp/forge": ["macos"],
    "Some/multi": ["macos", "linux"],
    "Some/spaced": ["macos / macos"],
}


class MergeGuardTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.config = root / "merge-guard.json"
        self.config.write_text(json.dumps(CONFIG), encoding="utf-8")
        self.fake_gh = root / "gh"
        self.fake_gh.write_text("#!/bin/sh\nprintf '%b' \"${FAKE_CHECKS:-}\"\n", encoding="utf-8")
        self.fake_gh.chmod(0o755)

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def run_guard(self, checks: str, *args: str, config: Path | None = None, **env: str):
        environment = {
            key: value
            for key, value in os.environ.items()
            if key != "GHAPP_ALLOW_UNVALIDATED_MERGE"
        }
        environment.update(
            {
                "FAKE_CHECKS": checks,
                "MERGE_GUARD_CONFIG": str(config or self.config),
                "MERGE_GUARD_GH": str(self.fake_gh),
                **env,
            }
        )
        return subprocess.run(
            ["/bin/bash", str(GUARD), *args],
            env=environment,
            cwd=self.tmp.name,
            text=True,
            capture_output=True,
            check=False,
        )

    def assert_rc(self, expected: int, checks: str, *args: str, **kwargs) -> None:
        result = self.run_guard(checks, *args, **kwargs)
        self.assertEqual(result.returncode, expected, result.stderr)

    def test_guarded_repo_merges_only_when_the_check_passed(self) -> None:
        forge = ("pr", "merge", "1", "--repo", "Generous-Corp/forge", "--squash")
        self.assert_rc(0, "macos\\tpass\\t1m\\turl\\n", *forge)
        for state in ("fail", "pending"):
            with self.subTest(state=state):
                self.assert_rc(1, f"macos\\t{state}\\t1m\\turl\\n", *forge)

    def test_a_check_that_has_not_reported_refuses(self) -> None:
        # The Forge #45 shape: --auto fired before the macOS run reported.
        forge = ("pr", "merge", "1", "--repo", "Generous-Corp/forge", "--squash")
        self.assert_rc(1, "other\\tpass\\t1m\\turl\\n", *forge)
        self.assert_rc(1, "", *forge)

    def test_auto_is_refused_on_a_guarded_repo_even_when_green(self) -> None:
        result = self.run_guard(
            "macos\\tpass\\t1m\\turl\\n",
            "pr", "merge", "1", "--repo", "Generous-Corp/forge", "--auto", "--squash",
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("merge-guard: refusing `--auto`", result.stderr)

    def test_unlisted_repo_passes_through(self) -> None:
        pulp = ("pr", "merge", "1", "--repo", "Generous-Corp/pulp")
        self.assert_rc(0, "macos\\tfail\\t1m\\turl\\n", *pulp, "--squash")
        self.assert_rc(0, "", *pulp, "--auto", "--squash")

    def test_every_configured_check_must_pass(self) -> None:
        multi = ("pr", "merge", "1", "--repo", "Some/multi", "--squash")
        self.assert_rc(0, "macos\\tpass\\t1m\\tu\\nlinux\\tpass\\t1m\\tu\\n", *multi)
        self.assert_rc(1, "macos\\tpass\\t1m\\tu\\nlinux\\tfail\\t1m\\tu\\n", *multi)
        self.assert_rc(
            0, "macos / macos\\tpass\\t1m\\tu\\n",
            "pr", "merge", "1", "--repo", "Some/spaced", "--squash",
        )

    def test_explicit_override_allows_and_says_so(self) -> None:
        result = self.run_guard(
            "macos\\tfail\\t1m\\turl\\n",
            "pr", "merge", "1", "--repo", "Generous-Corp/forge", "--squash",
            GHAPP_ALLOW_UNVALIDATED_MERGE="1",
        )
        self.assertEqual(result.returncode, 0)
        self.assertIn("OVERRIDDEN", result.stderr)

    def test_missing_or_malformed_config_is_inert(self) -> None:
        # It runs on every `pr merge`; blocking on its own broken config would
        # be worse than the problem it prevents.
        forge = ("pr", "merge", "1", "--repo", "Generous-Corp/forge", "--squash")
        missing = Path(self.tmp.name) / "absent.json"
        self.assert_rc(0, "", *forge, config=missing)
        broken = Path(self.tmp.name) / "broken.json"
        broken.write_text("bad json", encoding="utf-8")
        self.assert_rc(0, "", *forge, config=broken)

    def test_ghapp_dispatches_merge_guard_on_pr_merge(self) -> None:
        # Without this dispatch every case above is inert in practice.
        wrapper = WRAPPER.read_text(encoding="utf-8")
        self.assertIn('"$guards/merge-guard" "$@" || exit 1', wrapper)
        self.assertIn('"${1:-}" == "pr" && "${2:-}" == "merge"', wrapper)


if __name__ == "__main__":
    unittest.main()

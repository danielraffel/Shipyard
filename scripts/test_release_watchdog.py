#!/usr/bin/env python3
"""Decision tests for the release watchdog (auto-release-watchdog.yml) and cadence check."""

from __future__ import annotations

import re
import sys
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

import release_watchdog as rw  # noqa: E402

NOW = datetime(2026, 11, 1, 8, 0, tzinfo=timezone.utc)
PREFIX = "assign\tAssign the next version\t2026-11-01T07:59:23.5590535Z "


def ago(**delta: float) -> str:
    return (NOW - timedelta(**delta)).strftime("%Y-%m-%dT%H:%M:%SZ")


def merged(number: int, title: str, hours: float, sha: str) -> dict:
    return {"number": number, "title": title, "merged_at": ago(hours=hours), "merge_commit_sha": sha}


class ClassifyTests(unittest.TestCase):
    def test_classes_match_pulp(self) -> None:
        self.assertEqual(rw.classify("failure", 0), "workflow_file_error")
        self.assertEqual(rw.classify("startup_failure", 0), "workflow_file_error")
        self.assertEqual(rw.classify("failure", 1), "job_failure")
        self.assertEqual(rw.classify("timed_out", 1), "job_failure")
        self.assertEqual(rw.classify("success", 1), "success")
        self.assertEqual(rw.classify("cancelled", 1), "none")
        self.assertEqual(rw.classify("skipped", 0), "none")


class KnownFixTests(unittest.TestCase):
    def test_pat_without_pull_request_rights(self) -> None:
        fix = rw.known_fix("gh stderr: gh: Resource not accessible by personal access token (HTTP 403)")
        self.assertIn("Pull requests: Read and write", fix)
        self.assertIn("Commit statuses: Read and write", fix)
        self.assertIn("gh secret set RELEASE_BOT_TOKEN --repo danielraffel/Shipyard", fix)

    def test_expired_token(self) -> None:
        self.assertIn("expired", rw.known_fix("gh: Bad credentials (HTTP 401)"))
        self.assertIn("expired", rw.known_fix("fatal: could not read Username"))

    def test_missing_secret(self) -> None:
        self.assertIn("not set", rw.known_fix("RELEASE_BOT_TOKEN is required; refusing"))

    def test_ruleset_refusal(self) -> None:
        self.assertIn("repository rule",
                      rw.known_fix("remote: error: GH013: Repository rule violations found"))

    def test_unknown_error_points_at_the_log(self) -> None:
        self.assertIn("read the run log", rw.known_fix("something else"))


class LogExcerptTests(unittest.TestCase):
    def test_keeps_error_lines_and_drops_the_echoed_script(self) -> None:
        log = "\n".join(PREFIX + line for line in (
            "\x1b[36;1mtest -n \"$RELEASE_BOT_TOKEN\" || { echo 'RELEASE_BOT_TOKEN is required' >&2; }",
            "  RELEASE_BOT_TOKEN: ***",
            "version-at-land: gh api POST repos/o/r/pulls failed (exit 1)",
            "gh stderr: gh: Resource not accessible by personal access token (HTTP 403)",
            "##[error]Process completed with exit code 1.",
        ))
        excerpt = rw.log_excerpt(log)
        self.assertEqual(excerpt, [
            "version-at-land: gh api POST repos/o/r/pulls failed (exit 1)",
            "gh stderr: gh: Resource not accessible by personal access token (HTTP 403)",
            "##[error]Process completed with exit code 1.",
        ])
        self.assertIn("Pull requests", rw.known_fix("\n".join(excerpt)))


class WatchdogBodyTests(unittest.TestCase):
    def test_job_failure_names_run_error_and_fix(self) -> None:
        body = rw.watchdog_body("Assign versions at land", "job_failure", "https://run/1", "abc",
                                1, ["gh stderr: gh: Bad credentials (HTTP 401)"], NOW)
        self.assertIn("https://run/1", body)
        self.assertIn("gh stderr: gh: Bad credentials (HTTP 401)", body)
        self.assertIn("**Fix:** RELEASE_BOT_TOKEN is expired", body)
        self.assertIn("closes automatically", body)

    def test_workflow_file_error_points_at_actionlint(self) -> None:
        body = rw.watchdog_body("x", "workflow_file_error", "u", "s", 0, [], NOW)
        self.assertIn("actionlint", body)

    def test_each_watched_workflow_has_its_own_tracker(self) -> None:
        titles = {rw.watchdog_title(name) for name in rw.WATCHED}
        self.assertEqual(titles, {"version-at-land workflow failed: RELEASES BLOCKED",
                                  "auto-release workflow failed: RELEASES BLOCKED"})


class CadenceTests(unittest.TestCase):
    def test_untagged_fix_past_grace_is_flagged_oldest_first(self) -> None:
        prs = [merged(1105, "fix(steward): x", 6, "b"), merged(969, "feat(metrics): gate", 12, "a")]
        self.assertEqual([p["number"] for p in rw.untagged_releases(prs, {"a", "b"}, False, NOW, 2)],
                         [969, 1105])

    def test_inside_grace_is_not_flagged(self) -> None:
        self.assertEqual(rw.untagged_releases([merged(1, "fix: x", 1.5, "a")], {"a"}, False, NOW, 2), [])

    def test_tagged_merge_is_released(self) -> None:
        self.assertEqual(rw.untagged_releases([merged(1, "fix: x", 5, "a")], set(), False, NOW, 2), [])

    def test_open_bump_pr_is_waiting_not_stalled(self) -> None:
        self.assertEqual(rw.untagged_releases([merged(1, "fix: x", 5, "a")], {"a"}, True, NOW, 2), [])

    def test_only_feat_and_fix_need_a_release(self) -> None:
        prs = [merged(1, "docs: x", 5, "a"), merged(2, "chore: assign versions at land", 5, "b"),
               merged(3, "feat(cli)!: y", 5, "c"), merged(4, "Fix: z", 5, "d")]
        self.assertEqual([p["number"] for p in rw.untagged_releases(prs, {"a", "b", "c", "d"},
                                                                    False, NOW, 2)], [3, 4])

    def test_untagged_bump_past_grace_is_flagged(self) -> None:
        bumps = [{"sha": "a", "version": "0.384.0", "landed_at": ago(minutes=45), "tagged": False},
                 {"sha": "b", "version": "0.383.0", "landed_at": ago(hours=5), "tagged": True},
                 {"sha": "c", "version": "0.385.0", "landed_at": ago(minutes=5), "tagged": False}]
        self.assertEqual([b["version"] for b in rw.untagged_bumps(bumps, NOW, 30)], ["0.384.0"])

    def test_body_names_both_invariants(self) -> None:
        body = rw.cadence_body([merged(969, "feat: x", 12, "a")],
                               [{"sha": "f" * 40, "version": "1.2.3", "landed_at": ago(hours=1)}],
                               NOW, "o/r")
        self.assertIn("#969", body)
        self.assertIn("`v1.2.3`", body)
        self.assertIn("gh workflow run version-at-land.yml --repo o/r", body)


class TrackerTests(unittest.TestCase):
    def test_lifecycle(self) -> None:
        self.assertEqual(rw.issue_action(False, True), "create")
        self.assertEqual(rw.issue_action(True, True), "update")
        self.assertEqual(rw.issue_action(True, False), "close")
        self.assertEqual(rw.issue_action(False, False), "none")


class WorkflowTests(unittest.TestCase):
    def workflow(self, name: str) -> str:
        return (ROOT / ".github/workflows" / name).read_text()

    def test_watchdog_watches_the_release_chain_by_name(self) -> None:
        text = self.workflow("auto-release-watchdog.yml")
        for name in rw.WATCHED:
            source = next(p for p in (ROOT / ".github/workflows").glob("*.yml")
                          if p.read_text().startswith(f"name: {name}\n"))
            self.assertTrue(source.exists())
            self.assertIn(f'"{name}"', text)
        self.assertIn("release_watchdog.py watchdog", text)

    def test_cadence_runs_every_30_minutes_without_the_release_token(self) -> None:
        text = self.workflow("release-cadence-check.yml")
        self.assertIn('cron: "*/30 * * * *"', text)
        self.assertIn("fetch-depth: 0", text)
        self.assertNotIn("RELEASE_BOT_TOKEN", text)

    def test_least_privilege_and_concurrency(self) -> None:
        for name in ("auto-release-watchdog.yml", "release-cadence-check.yml"):
            text = self.workflow(name)
            self.assertIn("permissions: {}", text)
            self.assertIn("concurrency:", text)
            self.assertNotIn("contents: write", text)
            self.assertIsNone(re.search(r"pull-requests: write|actions: write", text))


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Decision tests for the release watchdog (auto-release-watchdog.yml) and cadence check."""

from __future__ import annotations

import contextlib
import io
import json
import re
import subprocess
import sys
import unittest
from unittest import mock
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


class FakeGitHub:
    """Models GitHub's label and issue rules: a label needs a colour, and an
    issue naming a label that does not exist is refused (422)."""

    def __init__(self, labels: set[str] | None = None, label_create_fails: bool = False) -> None:
        self.labels = set(labels or ())
        self.label_create_fails = label_create_fails
        self.issues: list[dict] = []
        self.calls: list[tuple[str, ...]] = []

    @staticmethod
    def fields(args: tuple[str, ...]) -> dict[str, list[str]]:
        out: dict[str, list[str]] = {}
        for flag, value in zip(args, args[1:]):
            if flag == "-f":
                key, _, val = value.partition("=")
                out.setdefault(key, []).append(val)
        return out

    def __call__(self, *args: str) -> subprocess.CompletedProcess[str]:
        self.calls.append(args)
        method = args[2] if args[1:2] == ("-X",) else "GET"
        path = args[3] if method != "GET" else args[1]
        f = self.fields(args)

        def done(rc: int, out: object = "") -> subprocess.CompletedProcess[str]:
            text = out if isinstance(out, str) else json.dumps(out)
            return subprocess.CompletedProcess(args, rc, stdout=text,
                                               stderr="" if rc == 0 else f"gh: HTTP {rc}")
        if method == "GET" and "/issues?" in path:
            want = "labels=release-stall" in path
            return done(0, [i for i in self.issues
                            if not want or "release-stall" in i["labels"]])
        if method == "POST" and path.endswith("/labels"):
            name = f["name"][0]
            if self.label_create_fails or "color" not in f:
                return done(1, {"message": "Validation Failed", "status": "422"})
            if name in self.labels:
                return done(1, {"message": "Validation Failed",
                                "errors": [{"code": "already_exists"}], "status": "422"})
            self.labels.add(name)
            return done(0, {"name": name})
        if method == "POST" and path.endswith("/issues"):
            labels = f.get("labels[]", [])
            if any(label not in self.labels for label in labels):
                return done(1, {"message": "Validation Failed", "status": "422"})
            issue = {"number": 1 + len(self.issues), "title": f["title"][0], "labels": labels}
            self.issues.append(issue)
            return done(0, issue)
        raise AssertionError(f"unexpected call {args}")


class TrackerCreationTests(unittest.TestCase):
    def apply(self, fake: FakeGitHub) -> tuple[str, str]:
        err = io.StringIO()
        with mock.patch.object(rw, "run_gh", fake), contextlib.redirect_stderr(err):
            action = rw.apply_tracker("o/r", "t", True, "body", "closed")
        return action, err.getvalue()

    def test_first_run_creates_coloured_labels_then_a_labelled_issue(self) -> None:
        fake = FakeGitHub()
        action, err = self.apply(fake)
        self.assertEqual(action, "create")
        creates = [FakeGitHub.fields(c) for c in fake.calls if c[3:4] == ("repos/o/r/labels",)]
        self.assertEqual([c["name"][0] for c in creates], list(rw.LABEL_SPECS))
        for c in creates:
            self.assertRegex(c["color"][0], r"^[0-9A-F]{6}$")
            self.assertTrue(c["description"][0])
        self.assertEqual(fake.issues[0]["labels"], list(rw.LABEL_SPECS))
        self.assertEqual(err, "")

    def test_existing_labels_are_fine(self) -> None:
        fake = FakeGitHub(labels=set(rw.LABEL_SPECS))
        self.assertEqual(self.apply(fake)[0], "create")
        self.assertEqual(fake.issues[0]["labels"], list(rw.LABEL_SPECS))

    def test_refused_labels_still_open_an_unlabelled_issue_and_say_so(self) -> None:
        fake = FakeGitHub(label_create_fails=True)
        action, err = self.apply(fake)
        self.assertEqual(action, "create")
        self.assertEqual(fake.issues[0]["labels"], [])
        self.assertIn("could not create label 'release-stall'", err)
        self.assertIn("without labels", err)

    def test_issue_refused_with_labels_falls_back_to_none(self) -> None:
        fake = FakeGitHub()
        real = fake.__call__

        def drop_one(*args: str) -> subprocess.CompletedProcess[str]:
            result = real(*args)
            fake.labels.discard("ci")  # a label vanishes between create and use
            return result
        err = io.StringIO()
        with mock.patch.object(rw, "run_gh", drop_one), contextlib.redirect_stderr(err):
            rw.apply_tracker("o/r", "t", True, "body", "closed")
        self.assertEqual(fake.issues[0]["labels"], [])
        self.assertIn("opening it without labels", err.getvalue())

    def test_unlabelled_tracker_is_found_by_title_not_duplicated(self) -> None:
        fake = FakeGitHub(label_create_fails=True)
        self.apply(fake)
        with mock.patch.object(rw, "run_gh", fake):
            found = rw.find_tracker("o/r", "t")
        self.assertEqual(found["number"], 1)


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

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
    def test_app_token_without_pull_request_rights(self) -> None:
        fix = rw.known_fix("gh stderr: gh: Resource not accessible by integration (HTTP 403)")
        self.assertIn("Pull requests: Read and write", fix)
        self.assertIn("Commit statuses: Read and write", fix)
        self.assertIn("shipyard-local App", fix)
        self.assertIn("gh workflow run version-at-land.yml --repo danielraffel/Shipyard", fix)

    def test_rejected_token(self) -> None:
        self.assertIn("rejected", rw.known_fix("gh: Bad credentials (HTTP 401)"))
        self.assertIn("rejected", rw.known_fix("fatal: could not read Username"))
        self.assertIn("SHIPYARD_APP_PRIVATE_KEY", rw.known_fix(
            "Release App token was rejected; check the secrets"))

    def test_missing_secret(self) -> None:
        fix = rw.known_fix("##[error]Shipyard release App credentials are required "
                           "(secret SHIPYARD_APP_PRIVATE_KEY); refusing")
        self.assertIn("not set", fix)
        self.assertIn("gh secret set SHIPYARD_APP_PRIVATE_KEY --repo danielraffel/Shipyard", fix)

    def test_ruleset_refusal(self) -> None:
        self.assertIn("repository rule",
                      rw.known_fix("remote: error: GH013: Repository rule violations found"))

    def test_unknown_error_points_at_the_log(self) -> None:
        self.assertIn("read the run log", rw.known_fix("something else"))


class LogExcerptTests(unittest.TestCase):
    def test_keeps_error_lines_and_drops_the_echoed_script(self) -> None:
        log = "\n".join(PREFIX + line for line in (
            "\x1b[36;1mtest -n \"$GH_TOKEN\" || { echo 'Release App token is required' >&2; }",
            "  GH_TOKEN: ***",
            "version-at-land: gh api POST repos/o/r/pulls failed (exit 1)",
            "gh stderr: gh: Resource not accessible by integration (HTTP 403)",
            "##[error]Process completed with exit code 1.",
        ))
        excerpt = rw.log_excerpt(log)
        self.assertEqual(excerpt, [
            "version-at-land: gh api POST repos/o/r/pulls failed (exit 1)",
            "gh stderr: gh: Resource not accessible by integration (HTTP 403)",
            "##[error]Process completed with exit code 1.",
        ])
        self.assertIn("Pull requests", rw.known_fix("\n".join(excerpt)))


class WatchdogBodyTests(unittest.TestCase):
    def test_job_failure_names_run_error_and_fix(self) -> None:
        body = rw.watchdog_body("Assign versions at land", "job_failure", "https://run/1", "abc",
                                1, ["gh stderr: gh: Bad credentials (HTTP 401)"], NOW)
        self.assertIn("https://run/1", body)
        self.assertIn("gh stderr: gh: Bad credentials (HTTP 401)", body)
        self.assertIn("**Fix:** The release App token was rejected", body)
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


ASSETS = ("shipyard-linux-x64", "shipyard-macos-arm64.dmg", "checksums.sha256")


def chain(**over: object) -> dict:
    """A v1.2.0 that landed two hours ago and released cleanly, with overrides."""
    args: dict = dict(
        version="1.2.0", landed_at=ago(hours=2), now=NOW, tag_exists=True,
        run={"id": 7, "html_url": "https://run/7", "status": "completed", "conclusion": "success"},
        failed_jobs="", release={"draft": False, "assets": [{"name": n} for n in ASSETS]},
        latest_tag="v1.2.0", expected_assets=ASSETS, bump_pr=None)
    args.update(over)
    return rw.chain_decide(**args)


def steps(verdict: dict) -> list[str]:
    return [step for step, _ in verdict["failures"]]


class ChainTests(unittest.TestCase):
    def test_a_clean_release_is_ok(self) -> None:
        self.assertEqual(chain()["state"], "ok")

    def test_inside_the_bound_nothing_is_judged(self) -> None:
        verdict = chain(landed_at=ago(minutes=30), tag_exists=False, run=None, release=None)
        self.assertEqual(verdict["state"], "pending")
        self.assertEqual(verdict["failures"], [])

    def test_past_the_bound_a_missing_tag_fails(self) -> None:
        verdict = chain(landed_at=ago(minutes=91), tag_exists=False, run=None, release=None)
        self.assertEqual(steps(verdict), ["tag_missing"])

    def test_no_release_run(self) -> None:
        self.assertEqual(steps(chain(run=None, release=None)),
                         ["release_run_missing", "release_unpublished"])

    def test_a_hung_release_run(self) -> None:
        verdict = chain(run={"id": 7, "status": "queued", "conclusion": None}, release=None)
        self.assertEqual(steps(verdict), ["release_run_incomplete", "release_unpublished"])

    def test_a_failed_install_check_names_the_job(self) -> None:
        verdict = chain(run={"id": 7, "html_url": "https://run/7", "status": "completed",
                             "conclusion": "failure"},
                        failed_jobs="sign-and-upload-macos -> Publish the attested release",
                        release=None)
        self.assertEqual(steps(verdict), ["release_run_failed", "release_unpublished"])
        self.assertIn("Publish the attested release", verdict["failures"][0][1])

    def test_a_failed_run_is_evidence_when_the_release_is_public_complete_and_latest(self) -> None:
        # Publication follows the install check; a later failed step (keychain
        # restore) or a hand recovery leaves an installable release.
        verdict = chain(run={"id": 7, "status": "completed", "conclusion": "failure"})
        self.assertEqual(verdict["state"], "ok")

    def test_a_fully_green_release_is_ok_inside_the_bound(self) -> None:
        self.assertEqual(chain(landed_at=ago(minutes=13))["state"], "ok")

    def test_inside_the_bound_an_unreleased_version_is_pending(self) -> None:
        verdict = chain(landed_at=ago(minutes=13), release=None,
                        run={"id": 7, "status": "in_progress", "conclusion": None})
        self.assertEqual(verdict["state"], "pending")

    def test_one_key_for_both_run_states(self) -> None:
        self.assertEqual(rw.chain_key("1.2.0", [("release_run_incomplete", ""),
                                                ("release_unpublished", "")]),
                         rw.chain_key("1.2.0", [("release_run_failed", ""),
                                                ("release_unpublished", "")]))

    def test_a_missing_tag_stops_there(self) -> None:
        verdict = chain(tag_exists=False)
        self.assertEqual(steps(verdict), ["tag_missing"])

    def test_a_missing_asset(self) -> None:
        verdict = chain(release={"draft": False, "assets": [{"name": "shipyard-linux-x64"}]})
        self.assertEqual(steps(verdict), ["release_assets_missing"])
        self.assertIn("shipyard-macos-arm64.dmg", verdict["failures"][0][1])

    def test_latest_must_be_this_tag(self) -> None:
        self.assertEqual(steps(chain(latest_tag="v1.1.0")), ["latest_mismatch"])

    def test_an_old_version_with_no_landing_time_is_judged(self) -> None:
        self.assertEqual(steps(chain(landed_at=None, tag_exists=False, run=None, release=None)),
                         ["tag_missing"])

    def test_a_bump_pr_open_past_two_hours_fails_even_inside_the_bound(self) -> None:
        pr = {"number": 1481, "created_at": ago(minutes=121), "verdict": "absent"}
        verdict = chain(bump_pr=pr, landed_at=ago(minutes=5), release=None, run=None)
        self.assertEqual(verdict["state"], "failing")
        self.assertEqual(steps(verdict), ["bump_pr_stale"])
        self.assertIn("#1481", verdict["failures"][0][1])
        self.assertIn("review-verdict at its head: absent", verdict["failures"][0][1])

    def test_a_bump_pr_under_two_hours_is_fine(self) -> None:
        # 8 of 82 healthy bumps took over 60 minutes; the limit is 120.
        pr = {"number": 1481, "created_at": ago(minutes=110), "verdict": "success"}
        self.assertEqual(chain(bump_pr=pr)["state"], "ok")

    def test_body_carries_the_key_the_notifier_compares(self) -> None:
        verdict = chain(latest_tag="v1.1.0")
        body = rw.chain_body("1.2.0", verdict, ["tag v1.2.0: present"], NOW, "o/r")
        self.assertTrue(body.startswith("<!-- release-blocked-key: v1.2.0:latest_mismatch -->"))
        self.assertIn("gh release edit v1.2.0 --latest --repo o/r", body)
        self.assertIn("- tag v1.2.0: present", body)

    def test_every_step_has_a_fix(self) -> None:
        self.assertTrue(set(rw.CHAIN_FIX) >= {
            "bump_pr_stale", "tag_missing", "release_run_missing", "release_run_incomplete",
            "release_run_failed", "release_unpublished", "release_assets_missing",
            "latest_mismatch"})

    def test_every_tracker_is_labelled_for_the_notifier(self) -> None:
        self.assertIn("release-blocked", rw.LABEL_SPECS)


class PickVersionTests(unittest.TestCase):
    BUMPS = [{"version": "1.4.0", "landed_at": ago(minutes=20)},
             {"version": "1.3.0", "landed_at": ago(minutes=70)},
             {"version": "1.2.0", "landed_at": ago(minutes=120)}]

    def test_a_current_version_past_its_bound_is_judged(self) -> None:
        self.assertEqual(rw.pick_version("1.2.0", self.BUMPS[2:], NOW), ("1.2.0", ago(minutes=120)))

    def test_rapid_landings_judge_the_newest_version_past_its_bound(self) -> None:
        # 2026-11-13: six bumps landed 30 to 80 minutes apart; judging only the
        # current one left a broken release.yml unreported for hours.
        self.assertEqual(rw.pick_version("1.4.0", self.BUMPS, NOW), ("1.2.0", ago(minutes=120)))

    def test_pending_only_when_nothing_has_passed_its_bound(self) -> None:
        self.assertEqual(rw.pick_version("1.4.0", self.BUMPS[:2], NOW), ("1.4.0", ago(minutes=20)))

    def test_a_version_older_than_the_lookback(self) -> None:
        self.assertEqual(rw.pick_version("1.1.0", self.BUMPS, NOW), ("1.1.0", None))

    def test_a_superseded_version_is_ok_once_a_newer_release_is_latest(self) -> None:
        verdict = chain(tag_exists=False, release=None, run=None, latest_tag="v1.3.0")
        self.assertEqual(verdict["state"], "ok")
        self.assertIn("superseded by v1.3.0", verdict["note"])

    def test_an_older_latest_does_not_count(self) -> None:
        self.assertEqual(steps(chain(latest_tag="v1.1.9")), ["latest_mismatch"])


class ChainIoDecisionTests(unittest.TestCase):
    def test_a_successful_run_outranks_a_later_failed_one(self) -> None:
        runs = [{"id": 9, "conclusion": "failure"}, {"id": 7, "conclusion": "success"}]
        self.assertEqual(rw.pick_run(runs)["id"], 7)
        self.assertEqual(rw.pick_run([{"id": 9, "conclusion": None}])["id"], 9)
        self.assertIsNone(rw.pick_run([]))

    def test_only_a_stale_bump_tracker_closes_while_pending(self) -> None:
        self.assertTrue(rw.stale_bump_only("<!-- release-blocked-key: v1.2.0:bump_pr_stale -->"))
        self.assertFalse(rw.stale_bump_only(
            "<!-- release-blocked-key: v1.2.0:bump_pr_stale,release_run_failed -->"))
        self.assertFalse(rw.stale_bump_only("<!-- release-blocked-key: v1.2.0:tag_missing -->"))
        self.assertFalse(rw.stale_bump_only("no marker"))


class RunChainTests(unittest.TestCase):
    """run_chain end to end with GitHub and git faked."""

    def run_chain(self, *, current: str, bumps: list[dict], releases: dict, latest: str,
                  tags: set[str], tracker: dict | None = None) -> tuple[dict, list[tuple]]:
        applied: list[tuple] = []

        def git(*args: str) -> subprocess.CompletedProcess[str]:
            rc = 0 if args[-1].removeprefix("refs/tags/") in tags else 1
            return subprocess.CompletedProcess(args, rc, stdout="", stderr="")

        def gh(*args: str, **kw: object) -> str:
            path = args[1]
            if "/pulls?" in path:
                return "[]"
            tag = re.search(r"branch=(v[\d.]+)", path)
            if tag and tag.group(1) in releases:
                return json.dumps({"workflow_runs": [{"id": 1, "html_url": "https://run/1",
                                                      "status": "completed",
                                                      "conclusion": "success"}]})
            return json.dumps({"workflow_runs": []})

        def optional(*args: str) -> dict | None:
            path = args[1]
            if path.endswith("/releases/latest"):
                return {"tag_name": latest}
            return releases.get(path.rsplit("/", 1)[-1])

        args = rw.argparse.Namespace(repo="o/r", ref="origin/main", lookback_days=14.0,
                                     bound_minutes=90.0, bump_stale_minutes=120.0,
                                     dry_run=False)
        out = io.StringIO()
        with mock.patch.object(rw, "cargo_version_at", return_value=current), \
                mock.patch.object(rw, "collect_bumps", return_value=bumps), \
                mock.patch.object(rw, "git", side_effect=git), \
                mock.patch.object(rw, "gh", side_effect=gh), \
                mock.patch.object(rw, "optional_json", side_effect=optional), \
                mock.patch("release_draft_heal.expected_assets", return_value=ASSETS), \
                mock.patch.object(rw, "find_tracker", return_value=tracker), \
                mock.patch.object(rw, "apply_tracker",
                                  side_effect=lambda *a: applied.append(a) or "x"), \
                contextlib.redirect_stdout(out):
            rw.run_chain(args, NOW)
        return json.loads(out.getvalue().splitlines()[0]), applied

    PUBLIC = {"draft": False, "html_url": "https://rel", "assets": [{"name": n} for n in ASSETS]}

    def test_back_to_back_bumps_cannot_hide_a_broken_release(self) -> None:
        # v1.3.0 landed 20 min ago; v1.2.0 landed 2 h ago and never published.
        bumps = [{"version": "1.3.0", "landed_at": ago(minutes=20)},
                 {"version": "1.2.0", "landed_at": ago(minutes=120)}]
        verdict, applied = self.run_chain(current="1.3.0", bumps=bumps, releases={},
                                          latest="v1.1.0", tags={"v1.2.0", "v1.3.0"})
        self.assertEqual(verdict["state"], "failing")
        self.assertEqual(verdict["version"], "v1.2.0")
        self.assertEqual(applied[0][1:3], (rw.CHAIN_TITLE, True))

    def test_a_green_current_release_closes_the_tracker_at_once(self) -> None:
        bumps = [{"version": "1.3.0", "landed_at": ago(minutes=20)}]
        verdict, applied = self.run_chain(current="1.3.0", bumps=bumps,
                                          releases={"v1.3.0": self.PUBLIC}, latest="v1.3.0",
                                          tags={"v1.3.0"}, tracker={"number": 4})
        self.assertEqual(verdict["state"], "ok")
        self.assertEqual(applied[0][1:3], (rw.CHAIN_TITLE, False))

    def test_pending_with_nothing_older_leaves_the_tracker(self) -> None:
        bumps = [{"version": "1.3.0", "landed_at": ago(minutes=20)}]
        verdict, applied = self.run_chain(current="1.3.0", bumps=bumps, releases={},
                                          latest="v1.2.0", tags={"v1.3.0"},
                                          tracker={"number": 4, "body": "x"})
        self.assertEqual(verdict["state"], "pending")
        self.assertEqual(applied, [])


class CadenceBumpCoverTests(unittest.TestCase):
    """2026-11-18, issue 1493: #1487 (fix:) merged at 11:46:20Z and moved only the
    plugin. version-at-land's #1488 bumped plugin 0.184.8 -> 0.184.9 and merged at
    12:49:58Z, so no CLI tag was due. The sweep at 14:02Z still said "merged but
    in no release tag"."""

    FIX = {"number": 1487, "title": "fix(release): page the lead when a version is not "
           "released and installable", "merged_at": "2026-11-18T11:46:20Z",
           "merge_commit_sha": "ee7266ab", "head": {"ref": "fix/release-chain-watch"}}
    BUMP = {"number": 1488, "title": "chore: assign versions at land (plugin 0.184.9)",
            "merged_at": "2026-11-18T12:49:58Z", "merge_commit_sha": "2dcadb01",
            "head": {"ref": "release/version-at-land"}}

    def sweep(self, pulls: list[dict]) -> dict:
        history = {"2dcadb01": {"ee7266ab", "2dcadb01"}, "origin/main": {"ee7266ab", "2dcadb01"}}

        def git(*args: str) -> subprocess.CompletedProcess[str]:
            if args[:2] == ("merge-base", "--is-ancestor"):
                rc = 0 if args[2] in history.get(args[3], set()) else 1
                return subprocess.CompletedProcess(args, rc, stdout="", stderr="")
            return subprocess.CompletedProcess(args, 0, stdout="", stderr="")  # no v* tag

        def gh(*args: str, **kw: object) -> str:
            return json.dumps(pulls) if "state=closed" in args[1] else "[]"
        out = io.StringIO()
        args = rw.argparse.Namespace(repo="danielraffel/Shipyard", lookback_days=7.0,
                                     grace_hours=2.0, bump_grace_minutes=30.0, dry_run=True)
        with mock.patch.object(rw, "git", side_effect=git), \
                mock.patch.object(rw, "gh", side_effect=gh), \
                mock.patch.object(rw, "collect_bumps", return_value=[]), \
                contextlib.redirect_stdout(out):
            rw.run_cadence(args, datetime(2026, 11, 18, 14, 2, tzinfo=timezone.utc))
        return json.loads(out.getvalue().splitlines()[0])

    def test_a_change_a_merged_plugin_only_bump_covers_is_released(self) -> None:
        self.assertEqual(self.sweep([self.BUMP, self.FIX])["untagged_merges"], [])

    def test_without_a_covering_bump_it_is_still_flagged(self) -> None:
        self.assertEqual(self.sweep([self.FIX])["untagged_merges"], [1487])

    def test_only_merged_bump_pull_requests_count(self) -> None:
        unmerged = dict(self.BUMP, merged_at=None)
        self.assertEqual(rw.landed_bumps([unmerged, self.FIX]), [])
        self.assertEqual(rw.landed_bumps([self.BUMP, self.FIX]), ["2dcadb01"])


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


class TrackerLabelTests(unittest.TestCase):
    def update(self, labels: list[dict]) -> list[tuple[str, ...]]:
        calls: list[tuple[str, ...]] = []

        def fake(*args: str) -> subprocess.CompletedProcess[str]:
            calls.append(args)
            out = json.dumps([{"number": 3, "title": "t", "labels": labels}]) \
                if args[1].startswith("repos/o/r/issues?") else "{}"
            return subprocess.CompletedProcess(args, 0, stdout=out, stderr="")
        with mock.patch.object(rw, "run_gh", fake):
            self.assertEqual(rw.apply_tracker("o/r", "t", True, "body", "closed"), "update")
        return [c for c in calls if c[3:4] == ("repos/o/r/issues/3/labels",)]

    def test_an_unlabelled_tracker_gains_the_notify_label_on_update(self) -> None:
        added = self.update([{"name": "release-stall"}])
        self.assertEqual(len(added), 1)
        self.assertIn("labels[]=release-blocked", added[0])

    def test_a_labelled_tracker_is_left_alone(self) -> None:
        self.assertEqual(self.update([{"name": "release-blocked"}]), [])


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

    def test_chain_runs_on_schedule_and_after_release_runs(self) -> None:
        text = self.workflow("release-cadence-check.yml")
        # Heal completing means a re-run was requested, not finished; the
        # re-run's own Release completion triggers the check.
        self.assertIn('workflows: ["Release"]', text)
        self.assertIn("--bump-stale-minutes 120", text)
        self.assertIn("release_watchdog.py \"${args[@]}\" chain --bound-minutes 90", text)
        self.assertIn("if: ${{ !cancelled() }}", text)
        for name in ("Release",):
            self.assertTrue(any(p.read_text().startswith(f"name: {name}\n")
                                for p in (ROOT / ".github/workflows").glob("*.yml")))

    def test_least_privilege_and_concurrency(self) -> None:
        for name in ("auto-release-watchdog.yml", "release-cadence-check.yml"):
            text = self.workflow(name)
            self.assertIn("permissions: {}", text)
            self.assertIn("concurrency:", text)
            self.assertNotIn("contents: write", text)
            self.assertIsNone(re.search(r"pull-requests: write|actions: write", text))


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
from __future__ import annotations

import contextlib
import io
import json
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock
from datetime import datetime, timedelta, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

import release_draft_heal as heal  # noqa: E402

NOW = datetime(2026, 11, 1, 12, 0, tzinfo=timezone.utc)


def run(conclusion="failure", status="completed", age_min=60, attempt=1):
    ts = (NOW - timedelta(minutes=age_min)).strftime("%Y-%m-%dT%H:%M:%SZ")
    return {"id": 7, "status": status, "conclusion": conclusion, "updated_at": ts, "run_attempt": attempt}


def decide(release={"draft": True}, run_=None, signing=True, **kw):
    return heal.decide(release, run_, now=NOW, signing_enabled=signing, **kw)


class DecideTests(unittest.TestCase):
    def test_published_is_noop(self) -> None:
        self.assertEqual(decide({"draft": False}, run()).action, "noop")

    def test_manual_publish_mode_is_noop(self) -> None:
        self.assertEqual(decide(run_=run(), signing=False).action, "noop")

    def test_in_flight_run_is_left_alone(self) -> None:
        self.assertEqual(decide(run_=run(status="in_progress")).action, "noop")

    def test_grace_window_protects_a_fresh_failure(self) -> None:
        self.assertEqual(decide(run_=run(age_min=5)).action, "noop")

    def test_stale_failure_reruns_failed_jobs(self) -> None:
        d = decide(run_=run(), failed_steps="sign -> Install Rust")
        self.assertEqual((d.action, d.run_id), ("rerun", 7))
        self.assertIn("Install Rust", d.reason)

    def test_cancelled_is_never_resumed(self) -> None:
        d = decide(run_=run("cancelled"))
        self.assertEqual(d.action, "fail")
        self.assertIn("was cancelled", d.reason)

    def test_timed_out_and_startup_failure_rerun(self) -> None:
        for c in ("timed_out", "startup_failure"):
            self.assertEqual(decide(run_=run(c)).action, "rerun")

    def test_heal_only_calls_rerun_cancel_and_comment_write_endpoints(self) -> None:
        src = (ROOT / "scripts" / "release_draft_heal.py").read_text()
        # Every write is a POST whose endpoint is named on the same line or
        # comes from the re-run endpoint tuple; none publishes or edits a release.
        self.assertEqual(src.count('"-X", "POST"'), 5)
        for endpoint in ("rerun-failed-jobs", "/cancel", "/rerun", "/comments", "/commits/{sha}/comments"):
            self.assertIn(endpoint, src)
        # A normal cancel only: force-cancel skips the keychain restore.
        self.assertNotIn("/force-cancel", src)
        for bad in ("-X\", \"PATCH", "/git/refs", "releases/", '"DELETE"'):
            self.assertNotIn(bad, src)

    def test_attempts_are_bounded_and_fail_loudly(self) -> None:
        d = decide(run_=run(attempt=3), failed_steps="sign -> Install Rust")
        self.assertEqual(d.action, "fail")
        self.assertIn("not retrying", d.reason)

    def test_success_with_remaining_draft_fails_loudly(self) -> None:
        self.assertEqual(decide(run_=run("success")).action, "fail")

    def test_draft_without_any_run_fails_loudly(self) -> None:
        self.assertEqual(decide(run_=None).action, "fail")

    def test_untagged_version_is_noop(self) -> None:
        self.assertEqual(decide(release=None, run_=None).action, "noop")

    def test_published_with_every_asset_is_healthy_after_an_exhausted_run(self) -> None:
        assets = ("a.dmg", "checksums.sha256")
        release = {"draft": False, "assets": [{"name": "a.dmg"}, {"name": "checksums.sha256"}]}
        d = decide(release, run(attempt=4), expected_assets=assets)
        self.assertEqual(d.action, "noop")
        self.assertIn("all 2 expected assets", d.reason)

    def test_published_with_missing_assets_fails_loudly(self) -> None:
        release = {"draft": False, "assets": [{"name": "a.dmg"}]}
        d = decide(release, run("success"), expected_assets=("a.dmg", "checksums.sha256"))
        self.assertEqual(d.action, "fail")
        self.assertIn("checksums.sha256", d.reason)

    def test_expected_assets_follow_the_release_pipeline(self) -> None:
        with mock.patch.dict("os.environ", {"CI_PLATFORM_FOCUS": "macos"}):
            assets = heal.expected_assets()
        self.assertIn("shipyard-macos-arm64.dmg", assets)
        self.assertIn("checksums.sha256", assets)

    def test_cargo_version_reads_package_table(self) -> None:
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "Cargo.toml"
            p.write_text('[dependencies]\nx = { version = "9" }\n[package]\nname = "s"\nversion = "1.2.3"\n')
            self.assertEqual(heal.cargo_version(p), "1.2.3")


class RerunErrorTests(unittest.TestCase):
    def _main(self, stderr: str | None) -> tuple[int, str]:
        """Run main() with a stale failed release run; the rerun POST fails with `stderr`."""
        stamp = (datetime.now(timezone.utc) - timedelta(hours=2)).strftime("%Y-%m-%dT%H:%M:%SZ")
        release_run = {"id": 7, "status": "completed", "conclusion": "failure", "updated_at": stamp, "run_attempt": 1}

        def fake_gh(*args: str) -> str:
            if "-X" in args:
                if stderr is None:
                    return ""
                raise subprocess.CalledProcessError(1, ["gh", *args], stderr=stderr)
            url = args[1]
            if "/jobs" in url:
                return json.dumps({"jobs": []})
            if "/releases" in url:
                return json.dumps([{"tag_name": "v1.2.3", "draft": True}])
            return json.dumps({"workflow_runs": [release_run]})

        with tempfile.TemporaryDirectory() as d:
            cargo = Path(d) / "Cargo.toml"
            cargo.write_text('[package]\nname = "x"\nversion = "1.2.3"\n')
            out, err = io.StringIO(), io.StringIO()
            argv = ["heal", "--repo", "o/r", "--cargo-toml", str(cargo)]
            with mock.patch.object(heal, "_gh", fake_gh), mock.patch.dict(
                "os.environ", {"CI_MACOS_SIGNING_ENABLED": "true"}
            ), mock.patch.object(sys, "argv", argv), contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                rc = heal.main()
        return rc, out.getvalue() + err.getvalue()

    def test_successful_rerun_exits_zero(self) -> None:
        self.assertEqual(self._main(None)[0], 0)

    def test_lost_race_already_running_is_benign(self) -> None:
        for msg in (
            "gh: Cannot rerun a workflow run that is already running (HTTP 409)",
            "gh: Workflow is already running (HTTP 422)",
        ):
            rc, _ = self._main(msg)
            self.assertEqual(rc, 0, msg)

    def test_other_statuses_fail_loudly_and_name_the_status(self) -> None:
        for status, msg in (
            (403, "Resource not accessible by integration"),
            (404, "Not Found"),
            (401, "Bad credentials"),
            (409, "Conflict with something else"),
        ):
            rc, out = self._main(f"gh: {msg} (HTTP {status})")
            self.assertNotEqual(rc, 0, status)
            self.assertIn(f"HTTP {status}", out)


class ReleaseLookupTests(unittest.TestCase):
    """The current release is found even behind a page of abandoned drafts."""

    ASSETS = ("shipyard-macos-arm64.dmg", "checksums.sha256")

    def _main(self, current: dict) -> tuple[int, str]:
        stamp = (datetime.now(timezone.utc) - timedelta(hours=2)).strftime("%Y-%m-%dT%H:%M:%SZ")
        exhausted = {"id": 7, "status": "completed", "conclusion": "failure",
                     "updated_at": stamp, "run_attempt": 4}
        drafts = [{"tag_name": f"v0.1.{i}", "draft": True, "assets": []} for i in range(3)]
        listing = drafts + [current]

        def fake_gh(*args: str) -> str:
            assert "-X" not in args, args
            url = args[1]
            if "/jobs" in url:
                return json.dumps({"jobs": []})
            if "/releases?" in url:
                page = int(url.split("page=")[-1])
                return json.dumps(listing[(page - 1) * 2:page * 2])
            return json.dumps({"workflow_runs": [exhausted]})

        with tempfile.TemporaryDirectory() as d:
            cargo = Path(d) / "Cargo.toml"
            cargo.write_text('[package]\nname = "x"\nversion = "1.2.3"\n')
            out = io.StringIO()
            argv = ["heal", "--repo", "o/r", "--cargo-toml", str(cargo)]
            with mock.patch.object(heal, "_gh", fake_gh), \
                    mock.patch.object(heal, "RELEASE_PAGE_SIZE", 2), \
                    mock.patch.object(heal, "expected_assets", lambda: self.ASSETS), \
                    mock.patch.dict("os.environ", {"CI_MACOS_SIGNING_ENABLED": "true"}), \
                    mock.patch.object(sys, "argv", argv), contextlib.redirect_stdout(out), \
                    contextlib.redirect_stderr(out):
                rc = heal.main()
        return rc, out.getvalue()

    def test_published_release_past_the_first_page_is_healthy(self) -> None:
        current = {"tag_name": "v1.2.3", "draft": False,
                   "assets": [{"name": n} for n in self.ASSETS]}
        rc, out = self._main(current)
        self.assertEqual(rc, 0, out)
        self.assertIn("v1.2.3: noop: release is published", out)

    def test_stranded_draft_past_the_first_page_still_fails(self) -> None:
        rc, out = self._main({"tag_name": "v1.2.3", "draft": True, "assets": []})
        self.assertEqual(rc, 1, out)
        self.assertIn("not retrying", out)
        self.assertIn("stuck as draft", out)


def _ts(minutes_ago: float) -> str:
    return (NOW - timedelta(minutes=minutes_ago)).strftime("%Y-%m-%dT%H:%M:%SZ")


def job(step_minutes_ago, *, name="sign-and-upload-macos", step="Build release binaries under a hard time limit",
        status="in_progress", job_id=11):
    steps = [{"name": "Set up job", "status": "completed", "conclusion": "success",
              "started_at": _ts(90), "completed_at": _ts(89)}]
    if step_minutes_ago is not None:
        steps.append({"name": step, "status": "in_progress", "started_at": _ts(step_minutes_ago)})
    return {"id": job_id, "name": name, "status": status, "steps": steps}


class StallSelectionTests(unittest.TestCase):
    TIMEOUTS = {"Build release binaries under a hard time limit": 25,
                "Sign, notarize, staple, and upload macOS DMG": 105}

    def find(self, jobs, history=lambda j, s: [120.0] * 10, timeouts=None):
        return heal.find_stall(jobs, now=NOW, history=history,
                               timeouts=self.TIMEOUTS if timeouts is None else timeouts)

    def test_threshold_is_the_largest_of_floor_median_and_step_timeout(self) -> None:
        self.assertEqual(heal.stall_threshold_secs([], None), 20 * 60)
        self.assertEqual(heal.stall_threshold_secs([600.0, 700.0, 800.0], None), 3 * 700)
        self.assertEqual(heal.stall_threshold_secs([60.0], 25), 30 * 60)
        self.assertEqual(heal.stall_threshold_secs([60.0, 100.0], None), 20 * 60)

    def test_only_an_over_threshold_step_is_selected(self) -> None:
        self.assertIsNone(self.find([job(29)]))  # build: 25 + 5 grace = 30
        stall = self.find([job(31)])
        self.assertIsNotNone(stall)
        self.assertEqual((stall.job_id, stall.step_name), (11, "Build release binaries under a hard time limit"))
        self.assertEqual(stall.threshold_secs, 30 * 60)

    def test_a_self_bounded_step_is_not_cut_short(self) -> None:
        # A notarization retry inside its own 105-minute bound is progress.
        sign = "Sign, notarize, staple, and upload macOS DMG"
        self.assertIsNone(self.find([job(60, step=sign)]))
        self.assertIsNotNone(self.find([job(111, step=sign)]))

    def test_slow_history_raises_the_threshold(self) -> None:
        unbounded = "Some step without a timeout"
        self.assertIsNotNone(self.find([job(25, step=unbounded)], history=lambda j, s: [300.0]))
        self.assertIsNone(self.find([job(25, step=unbounded)], history=lambda j, s: [600.0]))

    def test_a_run_moving_through_steps_is_left_alone_and_history_is_lazy(self) -> None:
        history = mock.Mock(return_value=[])
        # Long-running job, but its current step started 5 minutes ago.
        self.assertIsNone(self.find([job(5)], history=history))
        history.assert_not_called()

    def test_fail_closed_without_a_current_step(self) -> None:
        self.assertIsNone(self.find([job(None)]))
        self.assertIsNone(self.find([job(90, status="queued")]))
        two = job(90)
        two["steps"].append({"name": "x", "status": "in_progress", "started_at": _ts(90)})
        self.assertIsNone(self.find([two]))

    def test_any_job_can_stall(self) -> None:
        stall = self.find([job(3), job(45, name="build (linux-x64)", step="Build release binaries", job_id=12)])
        self.assertEqual(stall.job_id, 12)

    def test_attempt_cap_holds_for_stalls(self) -> None:
        stall = heal.Stall(11, "sign-and-upload-macos", "Build", 40 * 60, 30 * 60)
        self.assertEqual(heal.decide_stall(run(status="in_progress", attempt=1), stall).action, "cancel-rerun")
        self.assertEqual(heal.decide_stall(run(status="in_progress", attempt=2), stall).action, "cancel-rerun")
        capped = heal.decide_stall(run(status="in_progress", attempt=3), stall)
        self.assertEqual(capped.action, "fail")
        self.assertIn("not cancelling", capped.reason)
        self.assertIsNone(heal.decide_stall(run(status="in_progress"), None))
        self.assertIsNone(heal.decide_stall(run(status="completed"), stall))


class FakeGitHub:
    """A fake `gh api` for one release run (id 7) that models cancel and re-run."""

    SIGN = "Sign, notarize, staple, and upload macOS DMG"
    BUILD = "Build release binaries under a hard time limit"

    def __init__(self, *, step_minutes_ago: float, attempt: int = 1, step: str = BUILD,
                 settles: bool = True, failed_rerun_restarts: bool = True, rerun_fails_fast: bool = False,
                 issue: bool = True, workflow: str | None = None, history: bool = True,
                 status: str = "in_progress", conclusion: str | None = None, die_on_poll: int = 0,
                 poll_errors: int = 0, workflow_unreadable: bool = False,
                 cancel_refusal: str | None = None, fail_reads_after_rerun: bool = False,
                 attempt_after_rerun=None, markers: list | None = None, marker_read_fails: bool = False,
                 marker_write_fails: bool = False, die_on_cancel: bool = False,
                 die_on_accepted: bool = False) -> None:
        self.now = datetime.now(timezone.utc)
        self.step_minutes_ago = step_minutes_ago
        self.step = step
        self.state = {"status": status, "conclusion": conclusion, "run_attempt": attempt}
        self.settles = settles
        self.failed_rerun_restarts = failed_rerun_restarts
        self.rerun_fails_fast = rerun_fails_fast
        self.issue = issue
        self.workflow = workflow if workflow is not None else RELEASE_YML
        self.history = history
        self.die_on_poll = die_on_poll
        self.poll_errors = poll_errors
        self.workflow_unreadable = workflow_unreadable
        self.cancel_refusal = cancel_refusal
        self.fail_reads_after_rerun = fail_reads_after_rerun
        self.rerun_requested = False
        # What /runs/7 reports as run_attempt once a re-run was requested:
        # None leaves it alone, MISSING drops the key, anything else replaces it.
        self.attempt_after_rerun = attempt_after_rerun
        # Commit comments on the run's head commit: (author login, body).
        self.markers: list = list(markers or [])
        self.marker_read_fails = marker_read_fails
        self.marker_write_fails = marker_write_fails
        self.die_on_cancel = die_on_cancel
        self.die_on_accepted = die_on_accepted
        self.posts: list[str] = []
        self.reads: list[str] = []
        self.cancelled_at: str | None = None

    def stamp(self, minutes: float) -> str:
        return (self.now - timedelta(minutes=minutes)).strftime("%Y-%m-%dT%H:%M:%SZ")

    def _restart(self) -> None:
        if self.rerun_fails_fast:
            self.state.update(status="completed", conclusion="failure", run_attempt=self.state["run_attempt"] + 1)
        else:
            self.state.update(status="queued", conclusion=None, run_attempt=self.state["run_attempt"] + 1)

    def __call__(self, *args: str) -> str:
        if "-X" in args:
            url = args[3]
            self.posts.append(url)
            if url.endswith("/commits/tagsha/comments"):
                if self.marker_write_fails:
                    raise subprocess.CalledProcessError(1, ["gh"], stderr="Forbidden (HTTP 403)")
                body = args[args.index("-f") + 1].removeprefix("body=")
                if self.die_on_accepted and "cancel-accepted" in body:
                    raise RuntimeError("the heal runner went away after the cancel")
                self.markers.append(("github-actions[bot]", body))
                return "{}"
            if url.endswith("/cancel"):
                if self.die_on_cancel:
                    raise RuntimeError("the heal runner went away before the cancel")
                if self.cancel_refusal:
                    raise subprocess.CalledProcessError(1, ["gh"], stderr=self.cancel_refusal)
                self.cancelled_at = self.stamp(0)
                if self.settles:
                    self.state.update(status="completed", conclusion="cancelled")
            elif url.endswith("/rerun-failed-jobs"):
                self.rerun_requested = True
                if not self.failed_rerun_restarts:
                    raise subprocess.CalledProcessError(1, ["gh"], stderr="no failed jobs (HTTP 422)")
                self._restart()
            elif url.endswith("/jobs/11/rerun"):
                self._restart()
            return "{}"
        url = args[1]
        self.reads.append(url)
        if "/releases" in url:
            return json.dumps([{"tag_name": "v1.2.3", "draft": True}])
        if "/commits/tagsha/comments" in url:
            if self.marker_read_fails:
                raise subprocess.CalledProcessError(1, ["gh"], stderr="Bad Gateway (HTTP 502)")
            return json.dumps([{"user": {"login": login}, "body": body} for login, body in self.markers])
        if "/contents/.github/workflows/release.yml" in url:
            if self.workflow_unreadable:
                raise subprocess.CalledProcessError(1, ["gh"], stderr="Bad Gateway (HTTP 502)")
            return self.workflow
        if "/issues?" in url:
            return json.dumps([{"number": 42, "labels": [{"name": "release-blocked"}]}] if self.issue else [])
        if url.endswith("/runs/7"):
            if self.fail_reads_after_rerun and self.rerun_requested:
                raise subprocess.CalledProcessError(1, ["gh"], stderr="Bad Gateway (HTTP 502)")
            if self.die_on_poll:
                raise RuntimeError("the heal runner went away")
            if self.poll_errors:
                self.poll_errors -= 1
                raise subprocess.CalledProcessError(1, ["gh"], stderr="Bad Gateway (HTTP 502)")
            body = {"id": 7, "head_sha": "tagsha", **self.state}
            if self.rerun_requested and self.attempt_after_rerun is not None:
                if self.attempt_after_rerun is MISSING:
                    del body["run_attempt"]
                else:
                    body["run_attempt"] = None if self.attempt_after_rerun is NULL else self.attempt_after_rerun
            return json.dumps(body)
        if "/runs/7/jobs" in url:
            step = {"name": self.step, "started_at": self.stamp(self.step_minutes_ago)}
            if self.state["status"] == "in_progress":
                step["status"] = "in_progress"
                job = {"status": "in_progress", "conclusion": None}
            else:
                step.update(status="completed", conclusion="cancelled",
                            completed_at=self.cancelled_at or self.stamp(0))
                job = {"status": "completed", "conclusion": self.state["conclusion"]}
            return json.dumps({"jobs": [{"id": 11, "name": "sign-and-upload-macos", **job, "steps": [step]}]})
        if "/jobs" in url:
            steps = [{"name": self.BUILD, "conclusion": "success",
                      "started_at": "2026-11-01T00:00:00Z", "completed_at": "2026-11-01T00:02:00Z"},
                     {"name": self.SIGN, "conclusion": "success",
                      "started_at": "2026-11-01T00:03:00Z", "completed_at": "2026-11-01T00:05:34Z"}]
            return json.dumps({"jobs": [{"name": "sign-and-upload-macos", "steps": steps if self.history else []}]})
        if "status=success" in url:
            return json.dumps({"workflow_runs": [{"id": 100 + i} for i in range(10)]})
        # A finished run is past heal's 20-minute grace window.
        age = self.step_minutes_ago if self.state["status"] != "completed" else max(self.step_minutes_ago, 30)
        return json.dumps({"workflow_runs": [{"id": 7, "head_sha": "tagsha",
                                              "updated_at": self.stamp(age), **self.state}]})


MISSING = object()
NULL = object()
RELEASE_YML = (ROOT / ".github" / "workflows" / "release.yml").read_text(encoding="utf-8")


def heal_main(fake: FakeGitHub, *, dry_run: bool = False, enabled: bool = True,
              settle_secs: float = 0) -> tuple[int, str]:
    with tempfile.TemporaryDirectory() as d:
        cargo = Path(d) / "Cargo.toml"
        cargo.write_text('[package]\nname = "x"\nversion = "1.2.3"\n')
        out = io.StringIO()
        argv = ["heal", "--repo", "o/r", "--cargo-toml", str(cargo), *(["--dry-run"] if dry_run else [])]
        with mock.patch.object(heal, "_gh", fake), \
                mock.patch.object(heal.time, "sleep", lambda s: None), \
                mock.patch.object(heal, "CANCEL_SETTLE_SECS", settle_secs), \
                mock.patch.object(heal, "RERUN_CONFIRM_SECS", 0), \
                mock.patch.dict("os.environ", {"CI_MACOS_SIGNING_ENABLED": "true",
                                               heal.CANCEL_STALLED_ENV: "true" if enabled else ""}), \
                mock.patch.object(sys, "argv", argv), \
                contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
            rc = heal.main()
    return rc, out.getvalue()


class StallMainTests(unittest.TestCase):
    """main() end to end against a fake GitHub that models cancel and re-run."""

    def _main(self, *, dry_run: bool = False, enabled: bool = True, **kw):
        fake = FakeGitHub(**kw)
        rc, out = heal_main(fake, dry_run=dry_run, enabled=enabled)
        return rc, out, fake.posts

    def test_stalled_build_is_cancelled_rerun_and_reported(self) -> None:
        rc, out, posts = self._main(step_minutes_ago=45)
        self.assertEqual(rc, 0, out)
        self.assertEqual([p.rsplit("/", 1)[-1] for p in posts],
                         ["comments", "cancel", "comments", "rerun-failed-jobs", "comments"])
        self.assertEqual(posts[0], "repos/o/r/commits/tagsha/comments")
        self.assertIn("issues/42/comments", posts[-1])
        self.assertIn("attempt 2/3", out)

    def test_falls_back_to_rerunning_the_stalled_job(self) -> None:
        rc, out, posts = self._main(step_minutes_ago=45, failed_rerun_restarts=False, issue=False)
        self.assertEqual(rc, 0, out)
        self.assertIn("repos/o/r/actions/jobs/11/rerun", posts)

    def test_a_fast_failing_rerun_counts_as_restarted_and_is_not_rerun_again(self) -> None:
        fake = FakeGitHub(step_minutes_ago=45, rerun_fails_fast=True, issue=False)
        rc, out = heal_main(fake)
        self.assertEqual(rc, 0, out)
        self.assertEqual([p.rsplit("/", 1)[-1] for p in fake.posts], ["comments", "cancel", "comments", "rerun-failed-jobs"])
        self.assertEqual(fake.state["run_attempt"], 2)

    def test_unreadable_confirmation_never_sends_the_fallback(self) -> None:
        # Attempt 2: the first re-run starts attempt 3, which fails fast, and
        # every read after it fails. A second request could start attempt 4.
        fake = FakeGitHub(step_minutes_ago=45, attempt=2, rerun_fails_fast=True,
                          fail_reads_after_rerun=True, issue=False)
        rc, out = heal_main(fake)
        self.assertEqual(rc, 1, out)
        self.assertEqual([p.rsplit("/", 1)[-1] for p in fake.posts], ["comments", "cancel", "comments", "rerun-failed-jobs"])
        self.assertEqual(fake.state["run_attempt"], 3)
        self.assertIn("outcome is unknown", out)

    def test_fallback_needs_positive_proof_the_attempt_did_not_advance(self) -> None:
        # Attempt 2; the first re-run request is refused, then the fresh read
        # reports this run_attempt. Only an integer equal to 2 allows the fallback.
        for reported in (MISSING, NULL, 0, 1, "2", 2.0, True):
            with self.subTest(reported=reported):
                fake = FakeGitHub(step_minutes_ago=45, attempt=2, failed_rerun_restarts=False,
                                  attempt_after_rerun=reported, issue=False)
                rc, out = heal_main(fake)
                self.assertEqual(rc, 1, out)
                self.assertEqual([p.rsplit("/", 1)[-1] for p in fake.posts], ["comments", "cancel", "comments", "rerun-failed-jobs"])
                self.assertIn("outcome is unknown", out)
        fake = FakeGitHub(step_minutes_ago=45, attempt=2, failed_rerun_restarts=False,
                          attempt_after_rerun=2, issue=False)
        rc, out = heal_main(fake)
        self.assertIn("repos/o/r/actions/jobs/11/rerun", fake.posts)

    def test_an_api_error_after_the_cancel_does_not_abort_the_pass(self) -> None:
        fake = FakeGitHub(step_minutes_ago=45, poll_errors=2, issue=False)
        started = time.monotonic()
        rc, out = heal_main(fake, settle_secs=30)
        # The patched time.sleep must reach every wait (no real 20 s polls).
        self.assertLess(time.monotonic() - started, 5)
        self.assertEqual(rc, 0, out)
        self.assertIn("repos/o/r/actions/runs/7/rerun-failed-jobs", fake.posts)

    def test_a_cancel_that_does_not_settle_is_rerun_by_the_next_pass(self) -> None:
        fake = FakeGitHub(step_minutes_ago=45, settles=False, issue=False)
        rc, out = heal_main(fake)
        self.assertEqual((rc, len(fake.posts)), (1, 3))
        self.assertIn("next heal pass re-runs it", out)
        # The run stops later; the next pass finishes the job.
        fake.state.update(status="completed", conclusion="cancelled")
        rc, out = heal_main(fake)
        self.assertEqual(rc, 0, out)
        self.assertEqual(fake.posts[-1], "repos/o/r/actions/runs/7/rerun-failed-jobs")
        self.assertEqual(fake.state["run_attempt"], 2)

    def test_cancel_sent_then_pass_dies_then_next_pass_reruns(self) -> None:
        fake = FakeGitHub(step_minutes_ago=45, die_on_poll=1, issue=False)
        with self.assertRaises(RuntimeError):
            heal_main(fake)
        self.assertEqual(fake.posts, ["repos/o/r/commits/tagsha/comments", "repos/o/r/actions/runs/7/cancel",
                                      "repos/o/r/commits/tagsha/comments"])
        self.assertEqual(fake.state["conclusion"], "cancelled")
        fake.die_on_poll = 0
        rc, out = heal_main(fake)
        self.assertEqual(rc, 0, out)
        self.assertIn("resume-rerun", out)
        self.assertEqual(fake.posts[3:], ["repos/o/r/actions/runs/7/rerun-failed-jobs"])
        self.assertEqual(fake.state["run_attempt"], 2)

    def _cancelled_long(self, **kw) -> FakeGitHub:
        return FakeGitHub(step_minutes_ago=45, status="completed", conclusion="cancelled", issue=False, **kw)

    def test_a_maintainer_cancelled_stall_is_not_resumed(self) -> None:
        # A long cancelled step, but no heal marker: a maintainer (or the
        # concurrency group) cancelled it.
        fake = self._cancelled_long()
        rc, out = heal_main(fake)
        self.assertEqual((rc, fake.posts), (1, []), out)
        self.assertIn("no release-heal cancel marker", out)

    INTENT = "<!-- release-heal-cancel run=7 attempt=1 nonce=0123456789abcdef -->"
    ACCEPTED = "<!-- release-heal-cancel-accepted run=7 attempt=1 nonce=0123456789abcdef -->"

    def test_a_heal_cancelled_run_with_its_accepted_marker_is_resumed(self) -> None:
        fake = self._cancelled_long(markers=[("github-actions[bot]", self.INTENT),
                                             ("github-actions[bot]", self.ACCEPTED)])
        rc, out = heal_main(fake)
        self.assertEqual(rc, 0, out)
        self.assertEqual(fake.posts, ["repos/o/r/actions/runs/7/rerun-failed-jobs"])

    def test_an_intent_marker_alone_is_not_resumed(self) -> None:
        fake = self._cancelled_long(markers=[("github-actions[bot]", self.INTENT)])
        rc, out = heal_main(fake)
        self.assertEqual((rc, fake.posts), (1, []), out)

    def test_a_mismatched_nonce_is_not_resumed(self) -> None:
        other = self.ACCEPTED.replace("0123456789abcdef", "fedcba9876543210")
        fake = self._cancelled_long(markers=[("github-actions[bot]", self.INTENT), ("github-actions[bot]", other)])
        rc, out = heal_main(fake)
        self.assertEqual((rc, fake.posts), (1, []), out)

    def test_a_marker_must_name_this_run_and_attempt_and_come_from_the_workflow(self) -> None:
        for author, change in (
            ("github-actions[bot]", ("attempt=1", "attempt=2")),
            ("github-actions[bot]", ("run=7", "run=8")),
            ("someone", ("", "")),
        ):
            with self.subTest(author=author, change=change):
                markers = [(author, m.replace(*change) if change[0] else m) for m in (self.INTENT, self.ACCEPTED)]
                fake = self._cancelled_long(markers=markers)
                rc, out = heal_main(fake)
                self.assertEqual((rc, fake.posts), (1, []), out)

    def test_an_unreadable_marker_is_not_resumed(self) -> None:
        fake = self._cancelled_long(markers=[("github-actions[bot]", self.INTENT),
                                             ("github-actions[bot]", self.ACCEPTED)], marker_read_fails=True)
        rc, out = heal_main(fake)
        self.assertEqual((rc, fake.posts), (1, []), out)
        self.assertIn("unreadable; not resuming", out)

    def test_a_refused_cancel_then_a_maintainer_cancel_is_not_resumed(self) -> None:
        fake = FakeGitHub(step_minutes_ago=45, issue=False,
                          cancel_refusal="Resource not accessible by integration (HTTP 403)")
        self.assertEqual(heal_main(fake)[0], 1)
        fake.cancel_refusal = None
        fake.state.update(status="completed", conclusion="cancelled")  # a maintainer cancels
        rc, out = heal_main(fake)
        self.assertEqual(rc, 1, out)
        self.assertNotIn("rerun", " ".join(fake.posts))
        self.assertIn("no release-heal cancel marker", out)

    def test_an_already_stopping_cancel_is_not_rerun(self) -> None:
        fake = FakeGitHub(step_minutes_ago=45, issue=False, cancel_refusal="Cannot cancel (HTTP 409)")
        rc, out = heal_main(fake, settle_secs=5)
        self.assertEqual((rc, fake.posts), (1, ["repos/o/r/commits/tagsha/comments",
                                                "repos/o/r/actions/runs/7/cancel"]), out)

    def test_a_pass_that_dies_after_its_intent_is_not_resumed(self) -> None:
        for die in ("die_on_cancel", "die_on_accepted"):
            with self.subTest(die=die):
                fake = FakeGitHub(step_minutes_ago=45, issue=False, **{die: True})
                with self.assertRaises(RuntimeError):
                    heal_main(fake)
                setattr(fake, die, False)
                fake.state.update(status="completed", conclusion="cancelled")
                writes = len(fake.posts)
                rc, out = heal_main(fake)
                self.assertEqual(rc, 1, out)
                self.assertEqual(len(fake.posts), writes)

    def test_no_cancel_without_a_recorded_marker(self) -> None:
        fake = FakeGitHub(step_minutes_ago=45, marker_write_fails=True, issue=False)
        rc, out = heal_main(fake)
        self.assertEqual((rc, fake.posts), (1, ["repos/o/r/commits/tagsha/comments"]), out)
        self.assertIn("could not record the cancel intent", out)

    def test_a_cancel_of_a_moving_run_is_not_resumed(self) -> None:
        # A maintainer cancelled while the build was 3 minutes in: no stall trace.
        rc, out, posts = self._main(step_minutes_ago=3, status="completed", conclusion="cancelled")
        self.assertEqual((rc, posts), (1, []))
        self.assertIn("was cancelled; re-run it by hand", out)

    def test_a_cancelled_stall_is_not_resumed_with_the_variable_off(self) -> None:
        rc, out, posts = self._main(step_minutes_ago=45, status="completed", conclusion="cancelled",
                                    enabled=False)
        self.assertEqual((rc, posts), (1, []))
        self.assertIn("was cancelled", out)

    def test_resume_respects_the_attempt_cap(self) -> None:
        rc, out, posts = self._main(step_minutes_ago=45, status="completed", conclusion="cancelled", attempt=3)
        self.assertEqual((rc, posts), (1, []))
        self.assertIn("not retrying", out)

    def test_step_timeouts_come_from_the_runs_own_commit(self) -> None:
        # The tag's release.yml bounds signing at 115 (threshold 120); main's
        # 105 would give 110. 112 minutes in, with no green history, is inside.
        tag_yml = RELEASE_YML.replace("timeout-minutes: 105", "timeout-minutes: 115")
        self.assertNotEqual(tag_yml, RELEASE_YML)
        fake = FakeGitHub(step_minutes_ago=112, step=FakeGitHub.SIGN, workflow=tag_yml, history=False)
        rc, out = heal_main(fake)
        self.assertEqual((rc, fake.posts), (0, []), out)
        self.assertIn("repos/o/r/contents/.github/workflows/release.yml?ref=tagsha", fake.reads)

    def test_unreadable_workflow_judges_no_live_stall(self) -> None:
        # Signing 22 minutes in, with green history (154 s): one notarization
        # attempt may run 25 minutes, so the 20-minute floor alone must not decide.
        readable = FakeGitHub(step_minutes_ago=22, step=FakeGitHub.SIGN)
        self.assertEqual(heal_main(readable)[0], 0)
        self.assertEqual(readable.posts, [])
        fake = FakeGitHub(step_minutes_ago=22, step=FakeGitHub.SIGN, workflow_unreadable=True)
        rc, out = heal_main(fake)
        self.assertEqual((rc, fake.posts), (0, []), out)
        self.assertIn("skipping stall judgement", out)

    def test_unreadable_workflow_resumes_nothing(self) -> None:
        # A human cancelled 30 minutes into notarization.
        fake = FakeGitHub(step_minutes_ago=30, step=FakeGitHub.SIGN, status="completed",
                          conclusion="cancelled", workflow_unreadable=True)
        rc, out = heal_main(fake)
        self.assertEqual((rc, fake.posts), (1, []), out)
        self.assertNotIn("resume-rerun", out)
        self.assertIn("skipping stall judgement", out)

    def test_a_refused_cancel_is_reported_without_waiting(self) -> None:
        fake = FakeGitHub(step_minutes_ago=45, issue=False,
                          cancel_refusal="Resource not accessible by integration (HTTP 403)")
        started = time.monotonic()
        rc, out = heal_main(fake, settle_secs=5)
        self.assertLess(time.monotonic() - started, 2)
        self.assertEqual((rc, fake.posts), (1, ["repos/o/r/commits/tagsha/comments",
                                                "repos/o/r/actions/runs/7/cancel"]))
        self.assertIn("could not cancel", out)
        self.assertNotIn("has not stopped", out)

    def test_unknown_step_without_history_fails_closed(self) -> None:
        fake = FakeGitHub(step_minutes_ago=22, step="Renamed signing step", workflow="", history=False)
        rc, out = heal_main(fake)
        self.assertEqual((rc, fake.posts), (0, []), out)

    def test_a_step_inside_its_bound_is_not_touched(self) -> None:
        rc, out, posts = self._main(step_minutes_ago=10)
        self.assertEqual((rc, posts), (0, []))
        self.assertIn("still in_progress", out)

    def test_attempt_cap_means_no_cancel(self) -> None:
        rc, out, posts = self._main(step_minutes_ago=45, attempt=3)
        self.assertEqual((rc, posts), (1, []))
        self.assertIn("not cancelling", out)

    def test_cancel_is_off_unless_the_variable_is_true(self) -> None:
        rc, out, posts = self._main(step_minutes_ago=45, enabled=False)
        self.assertEqual((rc, posts), (1, []))
        self.assertIn("RELEASE_HEAL_CANCEL_STALLED", out)
        self.assertIn("stalled", out)

    def test_dry_run_never_cancels(self) -> None:
        rc, out, posts = self._main(step_minutes_ago=45, dry_run=True)
        self.assertEqual((rc, posts), (0, []))
        self.assertIn("cancel-rerun", out)


if __name__ == "__main__":
    unittest.main()

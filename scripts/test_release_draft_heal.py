#!/usr/bin/env python3
from __future__ import annotations

import contextlib
import io
import json
import subprocess
import sys
import tempfile
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
        self.assertEqual(src.count('"-X", "POST"'), 4)
        for endpoint in ("rerun-failed-jobs", "/cancel", "/rerun", "/comments"):
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


class StallMainTests(unittest.TestCase):
    """main() end to end against a fake GitHub that models cancel and re-run."""

    def _main(self, *, step_minutes_ago: float, attempt: int = 1, dry_run: bool = False,
              settles: bool = True, failed_rerun_restarts: bool = True, issue: bool = True,
              enabled: bool = True):
        now = datetime.now(timezone.utc)
        stamp = lambda m: (now - timedelta(minutes=m)).strftime("%Y-%m-%dT%H:%M:%SZ")  # noqa: E731
        state = {"status": "in_progress", "conclusion": None, "run_attempt": attempt}
        posts: list[str] = []
        green_jobs = {"jobs": [{"name": "sign-and-upload-macos", "steps": [
            {"name": "Build release binaries under a hard time limit", "conclusion": "success",
             "started_at": "2026-11-01T00:00:00Z", "completed_at": "2026-11-01T00:02:00Z"}]}]}

        def fake_gh(*args: str) -> str:
            if "-X" in args:
                url = args[3]
                posts.append(url)
                if url.endswith("/cancel"):
                    if settles:
                        state.update(status="completed", conclusion="cancelled")
                elif url.endswith("/rerun-failed-jobs"):
                    if not failed_rerun_restarts:
                        raise subprocess.CalledProcessError(1, ["gh"], stderr="no failed jobs (HTTP 422)")
                    state.update(status="queued", conclusion=None, run_attempt=state["run_attempt"] + 1)
                elif url.endswith("/jobs/11/rerun"):
                    state.update(status="queued", conclusion=None, run_attempt=state["run_attempt"] + 1)
                return "{}"
            url = args[1]
            if "/releases" in url:
                return json.dumps([{"tag_name": "v1.2.3", "draft": True}])
            if "/issues?" in url:
                return json.dumps([{"number": 42, "labels": [{"name": "release-blocked"}]}] if issue else [])
            if url.endswith("/runs/7"):
                return json.dumps({"id": 7, **state})
            if "/runs/7/jobs" in url:
                return json.dumps({"jobs": [{"id": 11, "name": "sign-and-upload-macos", "status": "in_progress",
                                             "steps": [{"name": "Build release binaries under a hard time limit",
                                                        "status": "in_progress",
                                                        "started_at": stamp(step_minutes_ago)}]}]})
            if "/jobs" in url:
                return json.dumps(green_jobs)
            if "status=success" in url:
                return json.dumps({"workflow_runs": [{"id": 100 + i} for i in range(10)]})
            return json.dumps({"workflow_runs": [{"id": 7, "updated_at": stamp(step_minutes_ago), **state}]})

        with tempfile.TemporaryDirectory() as d:
            cargo = Path(d) / "Cargo.toml"
            cargo.write_text('[package]\nname = "x"\nversion = "1.2.3"\n')
            out = io.StringIO()
            argv = ["heal", "--repo", "o/r", "--cargo-toml", str(cargo), *(["--dry-run"] if dry_run else [])]
            with mock.patch.object(heal, "_gh", fake_gh), \
                    mock.patch.object(heal.time, "sleep", lambda s: None), \
                    mock.patch.object(heal, "CANCEL_SETTLE_SECS", 0), \
                    mock.patch.object(heal, "RERUN_CONFIRM_SECS", 0), \
                    mock.patch.dict("os.environ", {"CI_MACOS_SIGNING_ENABLED": "true",
                                                   heal.CANCEL_STALLED_ENV: "true" if enabled else ""}), \
                    mock.patch.object(sys, "argv", argv), \
                    contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
                rc = heal.main()
        return rc, out.getvalue(), posts

    def test_stalled_build_is_cancelled_rerun_and_reported(self) -> None:
        rc, out, posts = self._main(step_minutes_ago=45)
        self.assertEqual(rc, 0, out)
        self.assertEqual([p.rsplit("/", 1)[-1] for p in posts], ["cancel", "rerun-failed-jobs", "comments"])
        self.assertIn("issues/42/comments", posts[-1])
        self.assertIn("attempt 2/3", out)

    def test_falls_back_to_rerunning_the_stalled_job(self) -> None:
        rc, out, posts = self._main(step_minutes_ago=45, failed_rerun_restarts=False, issue=False)
        self.assertEqual(rc, 0, out)
        self.assertIn("repos/o/r/actions/jobs/11/rerun", posts)

    def test_a_cancel_that_does_not_settle_fails_without_rerunning(self) -> None:
        rc, out, posts = self._main(step_minutes_ago=45, settles=False)
        self.assertEqual(rc, 1)
        self.assertEqual(len(posts), 1)
        self.assertIn("did not stop", out)

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

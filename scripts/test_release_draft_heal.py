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

    def test_heal_only_calls_rerun_write_endpoint(self) -> None:
        src = (ROOT / "scripts" / "release_draft_heal.py").read_text()
        self.assertEqual(src.count('"-X", "POST"'), 1)
        self.assertIn("rerun-failed-jobs", src)
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


if __name__ == "__main__":
    unittest.main()

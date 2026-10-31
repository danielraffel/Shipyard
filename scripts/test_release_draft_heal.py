#!/usr/bin/env python3
from __future__ import annotations

import sys
import unittest
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

    def test_cancelled_reruns_too(self) -> None:
        self.assertEqual(decide(run_=run("cancelled")).action, "rerun")

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

    def test_cargo_version_reads_package_table(self) -> None:
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "Cargo.toml"
            p.write_text('[dependencies]\nx = { version = "9" }\n[package]\nname = "s"\nversion = "1.2.3"\n')
            self.assertEqual(heal.cargo_version(p), "1.2.3")


if __name__ == "__main__":
    unittest.main()

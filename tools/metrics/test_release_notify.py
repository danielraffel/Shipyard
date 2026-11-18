#!/usr/bin/env python3
"""Tests for tools/metrics/release_notify.py (run: python3 tools/metrics/test_release_notify.py)."""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import release_notify as rn  # noqa: E402

LEAD = {"host": "m5s", "target": "A90D2C0A-A0E4-4045-9242-42F522E78E95",
        "socket": "/tmp/cmux-nightly.sock"}
ORCH_M3 = {"host": "local", "target": "654D7FD3-5762-47E1-9E06-3E6ED569FB76",
           "socket": "/tmp/cmux-nightly.sock"}
ORCH = {"host": "local", "target": "workspace:3", "socket": ""}


def issue(number: int = 1500, key: str | None = "v1.2.0:release_run_failed") -> dict:
    body = (f"<!-- release-blocked-key: {key} -->\n" if key else "") + (
        "_Auto-generated._\n\n**v1.2.0 is not released and installable.**\n\n"
        "- **release_run_failed**: release.yml run https://run/7 concluded failure\n"
        "  - Fix: re-run it\n")
    return {"number": number, "title": "Release blocked: the current version is not released",
            "html_url": f"https://github.com/o/r/issues/{number}", "body": body}


class TargetTests(unittest.TestCase):
    def test_parses_hosts_sockets_and_comments(self) -> None:
        targets, refused = rn.parse_targets(
            "# lead\nm5s A90D2C0A-A0E4-4045-9242-42F522E78E95 /tmp/cmux-nightly.sock\n"
            "local workspace:3  # m3\n\n")
        self.assertEqual(targets, [LEAD, ORCH])
        self.assertEqual(refused, [])

    def test_refuses_shell_syntax_and_bad_shapes(self) -> None:
        targets, refused = rn.parse_targets(
            "m5s surface:1;rm\nm5s\nm5s a b c\n-F/tmp/evil surface:1\nm5s --socket\n"
            "m5s surface:1 -oProxyCommand=x\n")
        self.assertEqual(targets, [])
        self.assertEqual(len(refused), 6)

    def test_remote_command_is_batch_ssh_and_reads_the_text_from_stdin(self) -> None:
        self.assertEqual(rn.command(LEAD), [
            "ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "--", "m5s", "cmux", "--socket",
            "/tmp/cmux-nightly.sock", "agent", "message", "A90D2C0A-A0E4-4045-9242-42F522E78E95", "--from",
            "shipyard-release-watch", "--", "-"])
        self.assertEqual(rn.command(ORCH)[:4], ["cmux", "agent", "message", "workspace:3"])

    def test_shipped_default_names_both_leads_by_stable_id(self) -> None:
        text = (Path(rn.HERE) / "release-notify-targets.txt").read_text()
        targets, refused = rn.parse_targets(text)
        self.assertEqual(targets, [ORCH_M3, LEAD])
        self.assertEqual(refused, [])
        for t in targets:  # surface:N refs change after a reboot
            self.assertNotRegex(t["target"], r"^(surface|workspace):\d+$")


class PlanTests(unittest.TestCase):
    def deliver_all(self, state: dict, issues: list[dict], targets: list[dict]) -> list[dict]:
        owed = rn.plan(issues, state, targets)
        for msg in owed:
            rn.record(state, msg, issues)
        rn.prune(state, issues, targets)
        return owed

    def test_a_new_issue_goes_to_every_target_once(self) -> None:
        state: dict = {}
        owed = self.deliver_all(state, [issue()], [LEAD, ORCH])
        self.assertEqual(len(owed), 2)
        self.assertIn("Release blocked", owed[0]["text"])
        self.assertIn("https://github.com/o/r/issues/1500", owed[0]["text"])
        self.assertIn("release_run_failed", owed[0]["text"])
        self.assertEqual(self.deliver_all(state, [issue()], [LEAD, ORCH]), [])

    def test_a_new_failing_step_is_news(self) -> None:
        state: dict = {}
        self.deliver_all(state, [issue()], [LEAD])
        owed = self.deliver_all(state, [issue(key="v1.2.0:latest_mismatch")], [LEAD])
        self.assertEqual([m["key"] for m in owed], ["#1500:v1.2.0:latest_mismatch"])

    def test_an_issue_without_a_marker_is_announced_once(self) -> None:
        state: dict = {}
        self.assertEqual(len(self.deliver_all(state, [issue(key=None)], [LEAD])), 1)
        self.assertEqual(self.deliver_all(state, [issue(key=None)], [LEAD]), [])

    def test_a_closed_issue_gets_one_resolved_message_then_is_forgotten(self) -> None:
        state: dict = {}
        self.deliver_all(state, [issue()], [LEAD])
        owed = self.deliver_all(state, [], [LEAD])
        self.assertEqual(len(owed), 1)
        self.assertIn("fixed or superseded", owed[0]["text"])
        self.assertEqual(state.get("reported"), {})
        self.assertEqual(self.deliver_all(state, [], [LEAD]), [])

    def test_a_failed_delivery_is_retried(self) -> None:
        state: dict = {}
        owed = rn.plan([issue()], state, [LEAD])
        self.assertEqual(len(owed), 1)  # not recorded: send failed
        self.assertEqual(len(rn.plan([issue()], state, [LEAD])), 1)


class RunTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.targets = self.dir / "targets"
        self.targets.write_text("m5s A90D2C0A-A0E4-4045-9242-42F522E78E95 /tmp/cmux-nightly.sock\n")

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def args(self, **over: object) -> argparse.Namespace:
        a = argparse.Namespace(state=str(self.dir / "state.json"),
                               summary=str(self.dir / "summary.log"), targets=str(self.targets),
                               repo="o/r", interval=600.0, ghapp="ghapp", cmux="cmux",
                               force=False, dry_run=False)
        for k, v in over.items():
            setattr(a, k, v)
        return a

    def fake(self, issues: list[dict], send_rc: int = 0, comment_rc: int = 0):
        calls: list[list[str]] = []

        def run(cmd, **kw):
            calls.append(cmd)
            if cmd[0] == "ghapp" and "-X" in cmd:
                return subprocess.CompletedProcess(cmd, comment_rc, stdout="{}",
                                                   stderr="" if comment_rc == 0 else "HTTP 502")
            if cmd[0] == "ghapp":
                return subprocess.CompletedProcess(cmd, 0, stdout=json.dumps(issues), stderr="")
            return subprocess.CompletedProcess(cmd, send_rc, stdout="",
                                               stderr="" if send_rc == 0 else "no such surface")
        return calls, run

    def test_sends_once_logs_and_respects_the_interval(self) -> None:
        calls, run = self.fake([issue()])
        with mock.patch.object(rn.subprocess, "run", run):
            lines = rn.run(self.args(), now=1000.0)
            self.assertTrue(any("release-notify sent=1 failed=0 open=1" in ln for ln in lines))
            self.assertEqual(rn.run(self.args(), now=1200.0), [])  # inside the interval
            rn.run(self.args(), now=2000.0)  # nothing new to say
        sends = [c for c in calls if c[0] == "ssh"]
        self.assertEqual(len(sends), 1)
        self.assertIn("sent=1", (self.dir / "summary.log").read_text())

    def test_an_unresolvable_target_falls_back_to_one_issue_comment(self) -> None:
        calls, run = self.fake([issue()], send_rc=1)
        with mock.patch.object(rn.subprocess, "run", run):
            first = rn.run(self.args(), now=1000.0)
            rn.run(self.args(), now=2000.0)
            third = rn.run(self.args(), now=3000.0)
            rn.run(self.args(), now=4000.0)  # delivered by comment: nothing more
        self.assertFalse(any("ERROR" in ln for ln in first))  # one failure is not news
        self.assertEqual(len([c for c in calls if c[0] == "ssh"]), 3)
        comments = [c for c in calls if c[0] == "ghapp" and "-X" in c]
        self.assertEqual(len(comments), 1)
        self.assertIn("repos/o/r/issues/1500/comments", comments[0])
        self.assertIn("could not reach", " ".join(comments[0]))
        self.assertIn("Release blocked", " ".join(comments[0]))
        self.assertTrue(any("unreachable after 3 tries" in ln and "commented on #1500" in ln
                            for ln in third))
        self.assertEqual((self.dir / "summary.log").read_text().count("unreachable"), 1)

    def test_a_target_that_never_resolves_and_no_comment_gives_up_after_a_day(self) -> None:
        calls, run = self.fake([issue()], send_rc=1, comment_rc=1)
        with mock.patch.object(rn.subprocess, "run", run):
            for n in range(5):
                rn.run(self.args(), now=1000.0 + n * 3600)
            lines = rn.run(self.args(), now=1000.0 + 25 * 3600)
            rn.run(self.args(), now=1000.0 + 26 * 3600)
        self.assertTrue(any("gave up" in ln for ln in lines))
        self.assertEqual(len([c for c in calls if c[0] == "ssh"]), 6)

    def test_failures_are_counted_per_message_and_target(self) -> None:
        state: dict = {}
        a = self.args()
        msg = rn.plan([issue()], state, [LEAD])[0]
        rn.on_failure(state, msg, [issue()], "x", 1.0, "s", a)
        rn.on_failure(state, msg, [issue()], "x", 2.0, "s", a)
        self.assertEqual(state["failures"][f"{msg['key']}@{rn.dest(LEAD)}"]["count"], 2)

    def test_a_github_read_error_is_logged_not_raised(self) -> None:
        def run(cmd, **kw):
            return subprocess.CompletedProcess(cmd, 1, stdout="", stderr="HTTP 502")
        with mock.patch.object(rn.subprocess, "run", run):
            lines = rn.run(self.args(), now=1000.0)
        self.assertTrue(any("ERROR read" in ln and "HTTP 502" in ln for ln in lines))

    def test_dry_run_sends_and_saves_nothing(self) -> None:
        calls, run = self.fake([issue()])
        with mock.patch.object(rn.subprocess, "run", run):
            lines = rn.run(self.args(dry_run=True), now=1000.0)
        self.assertTrue(any("would send #1500" in ln for ln in lines))
        self.assertFalse(any(c[0] == "ssh" for c in calls))
        self.assertFalse((self.dir / "state.json").exists())

    def test_tick_mode_never_fails(self) -> None:
        with mock.patch.object(rn, "run", side_effect=RuntimeError("boom")):
            self.assertEqual(rn.main(["--tick", "--log-dir", str(self.dir)]), 0)


if __name__ == "__main__":
    unittest.main()

"""Tests for watch_inputs.py over live captures (tests/fixtures/watch_inputs)."""

from __future__ import annotations

import importlib.util
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest
from datetime import datetime, timedelta, timezone

SCRIPT = pathlib.Path(__file__).with_name("watch_inputs.py")
SPEC = importlib.util.spec_from_file_location("watch_inputs", SCRIPT)
assert SPEC and SPEC.loader
inputs = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(inputs)

FIXTURES = pathlib.Path(__file__).resolve().parent.parent / "tests" / "fixtures" / "watch_inputs"
AT = datetime(2026, 10, 13, 2, 0, tzinfo=timezone.utc)
FIX_FIELDS = {"what", "evidence", "cause", "command", "safe_to_auto_apply", "verify"}


def screen(name: str) -> str:
    return (FIXTURES / name).read_text(encoding="utf-8")


def kinds(events: list[dict]) -> list[str]:
    return [item["kind"] for item in events]


class CodexScreens(unittest.TestCase):
    def classify(self, name: str, text: str | None = None) -> list[dict]:
        return inputs.classify_screen("workspace:9", "codex: test", text or screen(name), AT)

    def test_a_session_that_failed_to_resume_is_dead(self) -> None:
        events = self.classify("screen-1000010001.txt")
        self.assertEqual(kinds(events), ["codex_session_dead"])
        item = events[0]
        self.assertEqual(item["session"], "01a126c8-4a52-7933-8136-ffaf37eb8774")
        self.assertIn("Model provider `subrouter` not found", item["summary"])
        self.assertIn("cmux restore codex 01a126c8-4a52-7933-8136-ffaf37eb8774", item["fix"]["command"])
        self.assertFalse(item["fix"]["safe_to_auto_apply"])

    def test_a_session_that_exited_to_the_shell_is_dead(self) -> None:
        events = self.classify("screen-1000010022.txt")
        self.assertEqual(kinds(events), ["codex_session_dead"])
        self.assertEqual(events[0]["fix"]["cause"], "session_exited")
        self.assertIn("subrouter: signal: terminated", events[0]["summary"])

    def test_a_non_codex_screen_with_a_prompt_mark_above_a_shell_is_not_a_dead_session(self) -> None:
        text = ("$ ./build.sh\n› step 3 of 4: linking\nbuild failed\n"
                "danielraffel@Daniels-Mac-Studio-m3 pulp % \n")
        self.assertEqual(inputs.classify_screen("workspace:9", "build", text, AT), [])
        # Control: the same screen in a Codex workspace is a dead session.
        self.assertEqual(kinds(inputs.classify_screen("workspace:9", "codex: build", text, AT)),
                         ["codex_session_dead"])

    def test_the_reader_only_lists_and_reads_screens(self) -> None:
        listing = ("* workspace:1  optimize-import  [selected]\n"
                   "  workspace:2  codex: dead\n  workspace:3  codex: drafting\n")
        screens = {"workspace:1": screen("screen-1000010004.txt"),
                   "workspace:2": screen("screen-1000010001.txt"),
                   "workspace:3": screen("screen-1000010023.txt")}
        calls: list[list[str]] = []

        def runner(argv: list[str]) -> tuple[int, str]:
            calls.append(argv)
            if argv[1:] == ["workspace", "list"]:
                return 0, listing
            return 0, screens[argv[argv.index("--workspace") + 1]]

        events = inputs.read_cmux(runner, AT)
        self.assertEqual(sorted(kinds(events)), ["codex_draft_unsent", "codex_session_dead"])
        self.assertEqual(len(calls), 4)
        for argv in calls:
            self.assertEqual(argv[0], inputs.CMUX)
            self.assertIn(argv[1:3], (["workspace", "list"], ["read-screen", "--workspace"]), argv)
            self.assertFalse(any(word in ("send", "send-key", "paste") for word in argv), argv)

    def test_an_idle_or_working_session_with_an_empty_prompt_is_quiet(self) -> None:
        self.assertEqual(self.classify("screen-1000010004.txt"), [])
        self.assertEqual(self.classify("screen-1000010012.txt"), [])

    def test_an_unsent_multi_line_draft_is_reported_with_its_text(self) -> None:
        events = self.classify("screen-1000010023.txt")
        self.assertEqual(kinds(events), ["codex_draft_unsent"])
        item = events[0]
        self.assertTrue(item["summary"].endswith(item["summary"][-20:]))
        self.assertIn("Add to G6 (pre-push diff-cover)", item["summary"])
        self.assertEqual(item["fix"]["command"], "cmux send-key --workspace workspace:9 enter")
        self.assertFalse(item["fix"]["safe_to_auto_apply"], "someone may be typing it")
        # The whole draft decides the key, not just its first line.
        cut = screen("screen-1000010023.txt").replace("Test both", "Test one")
        self.assertNotEqual(self.classify("", cut)[0]["key"], item["key"])

    def test_a_session_still_retrying_a_503_is_left_alone(self) -> None:
        # Captured mid-retry ("Reconnecting... 2/5 ... esc to interrupt"): Codex
        # retries by itself, so "continue" would only interrupt it.
        self.assertEqual(self.classify("screen-1000010012-reconnecting.txt"), [])
        # Control: the same screen once the retries are spent and the turn ended.
        spent = "\n".join(line for line in screen("screen-1000010012-reconnecting.txt").splitlines()
                          if "esc to interrupt" not in line)
        events = self.classify("", spent)
        self.assertEqual(kinds(events), ["codex_session_errored"])
        self.assertIn("Unexpected status 503", events[0]["summary"])

    def test_an_idle_session_on_an_error_is_errored(self) -> None:
        # Synthetic: the captured idle layout with Codex's stream error above the prompt.
        text = screen("screen-1000010004.txt").replace(
            "  • No hosts changed; CI is pending.",
            "■ unexpected status 503 Service Unavailable: upstream connect error")
        events = self.classify("", text)
        self.assertEqual(kinds(events), ["codex_session_errored"])
        fix = events[0]["fix"]
        self.assertEqual(
            fix["command"],
            "cmux send --workspace workspace:9 continue && cmux send-key --workspace workspace:9 enter")
        self.assertFalse(fix["safe_to_auto_apply"], "typing into a session needs a steward")
        # The same error while the session is working again is not reported.
        working = screen("screen-1000010012.txt").replace(
            "include the first JUnit failure line", "■ unexpected status 503 Service Unavailable")
        self.assertEqual(self.classify("", working), [])

    def test_a_draft_waiting_to_queue_while_working_is_queued(self) -> None:
        # Synthetic: the captured working layout with a draft and Codex's queue hint.
        text = screen("screen-1000010012.txt").replace(
            "› Ask Codex to do anything", "› also check the m5 lane").replace(
            "? for shortcuts", "tab to queue message")
        events = self.classify("", text)
        self.assertEqual(kinds(events), ["codex_draft_queued"])
        self.assertEqual(events[0]["fix"]["command"], "cmux send-key --workspace workspace:9 tab")
        self.assertFalse(events[0]["fix"]["safe_to_auto_apply"], "typing into a session needs a steward")
        # Without the queue hint a draft in a working session is someone typing.
        typing = text.replace("tab to queue message", "? for shortcuts")
        self.assertEqual(self.classify("", typing), [])


class Hosts(unittest.TestCase):
    def status(self) -> dict:
        return json.loads((FIXTURES / "pool-status-m3.json").read_text(encoding="utf-8"))

    def test_each_named_problem_is_an_event(self) -> None:
        events = inputs.classify_pool("m3", self.status(), AT)
        self.assertEqual(kinds(events), ["host_problem", "host_problem"])
        sections = sorted(item["fix"]["cause"] for item in events)
        self.assertEqual(sections, ["self_update", "tool_freshness"])
        rolled = next(item for item in events if item["fix"]["cause"] == "self_update")
        self.assertIn("ROLLED BACK", rolled["summary"])
        self.assertIn("ssh m3 '~/.local/bin/tartci pool status'", rolled["fix"]["evidence"])
        local = inputs.classify_pool("local", self.status(), AT)[0]
        self.assertTrue(local["fix"]["evidence"].startswith("~/.local/bin/tartci pool status"))

    def test_a_host_out_of_the_pool_is_reported(self) -> None:
        status = self.status()
        status["state"] = "off"
        status["fleet"]["config"] = {}
        events = inputs.classify_pool("m5", status, AT)
        self.assertEqual(kinds(events), ["host_out_of_pool"])
        self.assertEqual(events[0]["key"], "host_out_of_pool:m5:off")
        status["state"] = "on"
        status["participating"] = False
        self.assertEqual(kinds(inputs.classify_pool("m5", status, AT)), ["host_out_of_pool"])

    def test_an_unreachable_host_is_reported_once_an_hour(self) -> None:
        def runner(argv: list[str]) -> tuple[int, str]:
            self.assertEqual(argv[:7], ["ssh", "-n", "-o", "BatchMode=yes", "-o",
                                        "ConnectTimeout=10", "m1"])
            return 255, "ssh: connect to host m1 port 22: Host is down"
        first = inputs.read_hosts(runner, ["m1"], AT)
        later = inputs.read_hosts(runner, ["m1"], AT + timedelta(minutes=30))
        self.assertEqual(kinds(first), ["host_unreachable"])
        self.assertEqual(first[0]["key"], later[0]["key"])
        self.assertEqual(first[0]["fix"]["cause"], "unreachable")

    def test_a_readable_host_reports_through_classify(self) -> None:
        body = json.dumps(self.status())
        events = inputs.read_hosts(lambda argv: (0, body), ["m3"], AT)
        self.assertEqual(kinds(events), ["host_problem", "host_problem"])


def job(host: str | None, created: int, started: int | None) -> dict:
    base = AT - timedelta(hours=3)
    return {"runner_name": f"{host}-pulp-gate-01-1-1" if host else "",
            "created_at": (base + timedelta(minutes=created)).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "started_at": None if started is None else
            (base + timedelta(minutes=started)).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "conclusion": None}


class GateServing(unittest.TestCase):
    HOSTS = ["studio", "m5studio", "m5", "m1"]

    def events(self, jobs: list[dict], minutes: int) -> list[dict]:
        at = AT - timedelta(hours=3) + timedelta(minutes=minutes)
        return inputs.serving_events(jobs, self.HOSTS, at, timedelta(minutes=10),
                                     timedelta(minutes=45), "o/r")

    def test_runner_names_map_to_their_host_prefix(self) -> None:
        self.assertEqual(inputs.host_of("m5studio-pulp-gate-slot2-02-94332-5", self.HOSTS), "m5studio")
        self.assertEqual(inputs.host_of("m5-pulp-gate-01-17249-175", self.HOSTS), "m5")
        self.assertEqual(inputs.host_of("studio-pulp-gate-01-21583-85", self.HOSTS), "studio")
        self.assertIsNone(inputs.host_of("GitHub Actions 1000285106", self.HOSTS))
        # Whatever the order, the dash keeps `m5` from claiming `m5studio`'s runners.
        self.assertEqual(inputs.host_of("m5studio-pulp-gate-01-1-1", ["m5", "m5studio"]), "m5studio")

    def test_a_host_absent_while_jobs_wait_is_not_serving(self) -> None:
        jobs = [job("studio", 0, 1), job("m5studio", 0, 1), job("m5", 0, 1), job("m1", 50, 51),
                job(None, 60, None)]
        events = self.events(jobs, 75)
        self.assertEqual(sorted(item["host"] for item in events), ["m5", "m5studio", "studio"])
        self.assertTrue(all(item["fix"]["safe_to_auto_apply"] is False for item in events))

    def test_a_job_cancelled_before_it_started_is_not_waiting(self) -> None:
        cancelled = job(None, 60, None)
        cancelled["completed_at"] = (AT - timedelta(hours=3) + timedelta(minutes=62)).strftime(
            "%Y-%m-%dT%H:%M:%SZ")
        self.assertEqual(self.events([job("studio", 0, 1), cancelled], 120), [])

    def test_no_waiting_job_means_no_event(self) -> None:
        jobs = [job("studio", 0, 1), job(None, 60, 62)]
        self.assertEqual(self.events(jobs, 120), [])
        # A job queued less than the wait is not waiting yet.
        self.assertEqual(self.events([job("studio", 0, 1), job(None, 115, None)], 120), [])

    def test_the_replay_emits_each_absence_once(self) -> None:
        jobs = [job("studio", 0, 1), job(None, 60, None)]
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "jobs.json"
            path.write_text(json.dumps(jobs), encoding="utf-8")
            start = (AT - timedelta(hours=2)).strftime("%Y-%m-%dT%H:%M:%SZ")
            end = AT.strftime("%Y-%m-%dT%H:%M:%SZ")
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "gate-serving", "--host", "studio",
                 "--from", start, "--to", end, "--jobs", str(path)],
                capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = [json.loads(line) for line in result.stdout.splitlines()]
        self.assertEqual(len(lines), 1)
        self.assertEqual(lines[0]["kind"], "host_not_serving")
        self.assertEqual(set(lines[0]["fix"]), FIX_FIELDS)


class EveryEventIsActionable(unittest.TestCase):
    def test_an_event_without_a_complete_fix_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            inputs.event("x", "k", AT, "s", {"what": "w"})

    def test_every_reader_fills_every_fix_field(self) -> None:
        items = (inputs.classify_screen("workspace:9", "t", screen("screen-1000010001.txt"), AT)
                 + inputs.classify_screen("workspace:9", "t", screen("screen-1000010023.txt"), AT)
                 + inputs.classify_pool("m3", json.loads(
                     (FIXTURES / "pool-status-m3.json").read_text(encoding="utf-8")), AT))
        for item in items:
            with self.subTest(kind=item["kind"]):
                self.assertEqual(set(item["fix"]), FIX_FIELDS)
                self.assertTrue(all(item["fix"][field] != "" for field in FIX_FIELDS))
                self.assertEqual(set(item) >= {"kind", "key", "at", "summary"}, True)


if __name__ == "__main__":
    unittest.main()

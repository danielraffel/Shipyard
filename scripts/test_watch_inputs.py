"""Tests for watch_inputs.py over live captures (tests/fixtures/watch_inputs)."""

from __future__ import annotations

import importlib.util
import json
import os
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


def pool_record(minutes: int, state: str = "on", participating: bool = True,
                heartbeat: bool = False, **problems: str) -> dict:
    """A record as tartci's pool-log agent writes it (scripts/pool_log.py)."""
    record = {"at": (AT + timedelta(minutes=minutes)).strftime("%Y-%m-%dT%H:%M:%SZ"),
              "host": "Daniels-Mac-Studio-m1", "state": state,
              "participating": participating, "problems": problems}
    if heartbeat:
        record["heartbeat"] = True
    return record


class PoolLogReplay(unittest.TestCase):
    """A pool-log replays as host events, each when its condition began."""

    def replay(self, records: list[dict]) -> tuple[list[dict], int]:
        parsed = [{**record, "_at": inputs.parse_time(record["at"])} for record in records]
        return inputs.replay_pool_log("m1", parsed)

    def test_each_condition_fires_when_it_begins_and_again_only_after_it_cleared(self) -> None:
        stale = "pulp 24.7 h behind"
        events, gaps = self.replay([
            pool_record(0),
            pool_record(10, tool_freshness=stale),
            pool_record(70, heartbeat=True, tool_freshness=stale),
            pool_record(130),
            pool_record(140, tool_freshness=stale),
            pool_record(150, state="off", participating=False, tool_freshness=stale),
            {"at": (AT + timedelta(minutes=160)).strftime("%Y-%m-%dT%H:%M:%SZ"),
             "error": "pool status exited 1: tartci: helper interpreter failed verification"},
            {"at": (AT + timedelta(minutes=165)).strftime("%Y-%m-%dT%H:%M:%SZ"),
             "error": "pool status exited 1: tartci: helper interpreter failed verification"},
            pool_record(170),
            pool_record(420, heartbeat=True),
        ])
        self.assertEqual([(item["kind"], item["at"][11:16]) for item in events], [
            ("host_problem", "02:10"),
            ("host_problem", "04:20"),
            ("host_out_of_pool", "04:30"),
            ("host_unreachable", "04:40"),
        ])
        self.assertEqual(gaps, 1, "170 -> 420 minutes: the agent was down")
        for item in events:
            self.assertEqual(item["host"], "m1")
            self.assertFalse(item["fix"]["safe_to_auto_apply"])
        self.assertIn("tool_freshness", events[0]["summary"])

    def test_a_problem_whose_hour_count_drifts_is_one_condition(self) -> None:
        records = [pool_record(index * 5, tool_freshness=f"pulp {24.8 + index * 0.1:.1f} h behind")
                   for index in range(12)]
        events, _ = self.replay(records)
        self.assertEqual(kinds(events), ["host_problem"], "one condition, not twelve")
        # Live passes key the same condition the same way, so the ledger dedupes it too.
        live = [inputs.classify_pool("m1", {"state": "on", "participating": True, "fleet": {
            "config": {"tool_freshness": {"problem": f"pulp {hours} h behind"}}}}, AT)[0]["key"]
            for hours in ("24.8", "25.9")]
        self.assertEqual(live[0], live[1])
        self.assertEqual(live[0], events[0]["key"])
        # Control: other words are another condition.
        other = inputs.classify_pool("m1", {"state": "on", "participating": True, "fleet": {
            "config": {"tool_freshness": {"problem": "vellum 24.8 h behind"}}}}, AT)[0]["key"]
        self.assertNotEqual(other, live[0])

    def test_duplicate_lines_are_read_once(self) -> None:
        line = json.dumps(pool_record(10, gate_reserve="low")) + "\n"
        with tempfile.TemporaryDirectory() as directory:
            first, second = pathlib.Path(directory) / "a.jsonl", pathlib.Path(directory) / "b.jsonl"
            first.write_text(json.dumps(pool_record(0)) + "\n" + line + line)
            second.write_text(line)
            records = inputs.read_pool_log([str(first), str(second)])
        self.assertEqual([record["at"][11:16] for record in records], ["02:00", "02:10"])

    def test_the_cli_replays_both_rotated_files_in_time_order(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            older, newer = root / "events.jsonl.1", root / "events.jsonl"
            older.write_text("".join(json.dumps(r) + "\n" for r in (
                pool_record(0), pool_record(10, gate_reserve="cores demand 6 against reserve 3"))))
            newer.write_text("".join(json.dumps(r) + "\n" for r in (
                pool_record(70, heartbeat=True, gate_reserve="cores demand 6 against reserve 3"),
                pool_record(80, state="off", participating=False))))
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "hosts", "--replay-log", f"m1={newer}",
                 "--replay-log", f"m1={older}"],
                capture_output=True, text=True, check=False, stdin=subprocess.DEVNULL)
            self.assertEqual(result.returncode, 0, result.stderr)
            lines = [json.loads(line) for line in result.stdout.splitlines()]
            self.assertEqual([item["kind"] for item in lines], ["host_problem", "host_out_of_pool"])
            self.assertIn("m1: 2 event(s) from 2 log file(s); 0 gap(s)", result.stderr)
            both = subprocess.run(
                [sys.executable, str(SCRIPT), "hosts", "--host", "m1", "--replay-log", f"m1={newer}"],
                capture_output=True, text=True, check=False, stdin=subprocess.DEVNULL)
            self.assertEqual(both.returncode, 2, both.stderr)
            bad = root / "bad.jsonl"
            bad.write_text('{"state": "on"}\n')
            refused = subprocess.run(
                [sys.executable, str(SCRIPT), "hosts", "--replay-log", f"m1={bad}"],
                capture_output=True, text=True, check=False, stdin=subprocess.DEVNULL)
            self.assertNotEqual(refused.returncode, 0)
            self.assertIn("bad.jsonl:1: a pool-log record needs `at`", refused.stderr)


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

    def test_gate_jobs_are_read_through_ghapp_never_a_bare_gh(self) -> None:
        calls: list[list[str]] = []

        def runner(argv: list[str]) -> tuple[int, str]:
            calls.append(argv)
            if "--paginate" in argv:
                return 0, json.dumps([{"workflow_runs": [{"id": 11}]}])
            return 0, json.dumps([job("studio", 0, 1)])

        jobs = inputs.read_gate_jobs(runner, "o/r", "build.yml", AT - timedelta(hours=1), AT)
        self.assertEqual(len(jobs), 1)
        self.assertEqual(len(calls), 2)
        for argv in calls:
            self.assertEqual(argv[0], inputs.GH, argv)
        self.assertTrue(os.path.isabs(inputs.GH), inputs.GH)
        if "WATCH_INPUTS_GH" not in os.environ:
            self.assertTrue(inputs.GH.endswith("/.local/bin/ghapp"), inputs.GH)

    def test_the_github_reader_is_ghapp_or_a_named_pinned_binary(self) -> None:
        self.assertEqual(inputs.gh_binary({"HOME": "/h"}), "/h/.local/bin/ghapp")
        self.assertEqual(inputs.gh_binary({"HOME": "/h", "WATCH_INPUTS_GH": "/opt/gh/2.60/gh"}),
                         "/opt/gh/2.60/gh")

    def test_the_cli_never_runs_the_gh_on_path(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            (root / "bin").mkdir()
            log = root / "calls.log"
            fake = ("#!/bin/sh\nprintf '%s %s\\n' \"$(basename \"$0\")\" \"$*\" >> " + str(log) + "\n"
                    "case \"$*\" in *--paginate*) echo '[{\"workflow_runs\": []}]';; *) echo '[]';; esac\n")
            for name in ("gh", "ghapp"):
                path = root / "bin" / name
                path.write_text(fake, encoding="utf-8")
                path.chmod(0o755)
            env = {**os.environ, "HOME": str(root), "PATH": f"{root / 'bin'}:{os.environ['PATH']}"}
            env.pop("WATCH_INPUTS_GH", None)
            (root / ".local" / "bin").mkdir(parents=True)
            (root / ".local" / "bin" / "ghapp").symlink_to(root / "bin" / "ghapp")
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "gate-serving", "--host", "studio",
                 "--from", "2026-10-12T00:00:00Z", "--to", "2026-10-12T00:10:00Z"],
                capture_output=True, text=True, check=False, env=env)
            self.assertEqual(result.returncode, 0, result.stderr)
            calls = log.read_text(encoding="utf-8").splitlines()
        self.assertTrue(calls, "the reader read nothing")
        self.assertTrue(all(line.startswith("ghapp api ") for line in calls), calls)

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

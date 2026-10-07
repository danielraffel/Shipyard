#!/usr/bin/env python3
"""Tests for hooks/handback-inbox.py (the PR hand-back inbox reader)."""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
HOOK = ROOT / "hooks" / "handback-inbox.py"
SESSION = "cfc73f94-128a-4c3d-8e69-2f278ff4fd8b"


def entry(pr: int, episode: str = "2026-09-29T01:00:00Z") -> str:
    return json.dumps(
        {
            "schema": "shipyard.pr-watch.handback/v1",
            "id": f"{pr}:repeat_test_failure:macos|t@{episode}",
            "repo": "o/r",
            "pr": pr,
            "url": f"https://github.com/o/r/pull/{pr}",
            "kind": "repeat_test_failure",
            "verdict": "code failure, not flake",
            "evidence": "`consumption-census-drift` failed on 2 runs " + "x" * 400,
        }
    )


class HandbackInboxHookTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def run_hook(self, payload: object, raw: str | None = None) -> subprocess.CompletedProcess:
        env = dict(os.environ, SHIPYARD_INBOX_DIR=str(self.dir))
        return subprocess.run(
            [sys.executable, str(HOOK)],
            input=raw if raw is not None else json.dumps(payload),
            capture_output=True,
            text=True,
            env=env,
            timeout=30,
            check=False,
        )

    def payload(self, event: str = "UserPromptSubmit") -> dict:
        return {"session_id": SESSION, "hook_event_name": event}

    def test_absent_inbox_is_a_silent_no_op(self) -> None:
        result = self.run_hook(self.payload())
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")

    def test_empty_inbox_is_a_silent_no_op(self) -> None:
        (self.dir / f"{SESSION}.jsonl").write_text("", encoding="utf-8")
        result = self.run_hook(self.payload())
        self.assertEqual((result.returncode, result.stdout), (0, ""))
        self.assertTrue((self.dir / f"{SESSION}.jsonl").exists())

    def test_entries_are_shown_once_then_moved_to_shown(self) -> None:
        inbox = self.dir / f"{SESSION}.jsonl"
        inbox.write_text(entry(9060) + "\nnot json\n" + entry(9055) + "\n", encoding="utf-8")
        result = self.run_hook(self.payload("SessionStart"))
        self.assertEqual(result.returncode, 0)
        output = json.loads(result.stdout)
        context = output["hookSpecificOutput"]["additionalContext"]
        self.assertEqual(output["hookSpecificOutput"]["hookEventName"], "SessionStart")
        self.assertIn("#9060", context)
        self.assertIn("#9055", context)
        self.assertIn("https://github.com/o/r/pull/9060", context)
        self.assertFalse(inbox.exists())
        shown = (self.dir / f"{SESSION}.shown.jsonl").read_text(encoding="utf-8")
        self.assertIn("9060", shown)
        # A second turn shows nothing.
        again = self.run_hook(self.payload())
        self.assertEqual(again.stdout, "")
        # The same entry re-queued (a duplicate delivery) is not shown again.
        inbox.write_text(entry(9060) + "\n", encoding="utf-8")
        dup = self.run_hook(self.payload())
        self.assertEqual(dup.stdout, "")

    def test_summary_is_bounded(self) -> None:
        lines = "".join(entry(9000 + n) + "\n" for n in range(12))
        (self.dir / f"{SESSION}.jsonl").write_text(lines, encoding="utf-8")
        result = self.run_hook(self.payload())
        context = json.loads(result.stdout)["hookSpecificOutput"]["additionalContext"]
        self.assertLessEqual(len(context), 2000)
        self.assertIn("and 7 more", context)
        for line in context.splitlines():
            self.assertLessEqual(len(line), 300)

    def test_bad_input_never_fails_the_turn(self) -> None:
        for raw in ["not json", "[]", json.dumps({"session_id": "../../etc/passwd"}), ""]:
            result = self.run_hook(None, raw=raw)
            self.assertEqual((result.returncode, result.stdout), (0, ""), raw)

    def test_other_sessions_inbox_is_not_read(self) -> None:
        other = self.dir / "someone-else.jsonl"
        other.write_text(entry(1) + "\n", encoding="utf-8")
        result = self.run_hook(self.payload())
        self.assertEqual(result.stdout, "")
        self.assertTrue(other.exists())

    def test_plugin_registers_the_hook_for_both_events(self) -> None:
        hooks = json.loads((ROOT / "hooks" / "hooks.json").read_text(encoding="utf-8"))["hooks"]
        for event in ("SessionStart", "UserPromptSubmit"):
            commands = [h["command"] for group in hooks[event] for h in group["hooks"]]
            self.assertTrue(
                any("handback-inbox.py" in command for command in commands), event
            )


    def test_displayed_entries_are_stamped_with_shown_at(self) -> None:
        (self.dir / f"{SESSION}.jsonl").write_text(entry(9001) + "\n", encoding="utf-8")
        result = self.run_hook(self.payload())
        self.assertIn("#9001", result.stdout)
        shown = (self.dir / f"{SESSION}.shown.jsonl").read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(shown), 1)
        record = json.loads(shown[0])
        self.assertEqual(record["id"], json.loads(entry(9001))["id"])
        self.assertRegex(record["shown_at"], r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$")


class HandbackInboxPollTests(unittest.TestCase):
    """hooks/handback-inbox-poll.sh: the PostToolUse entry point."""

    POLL = ROOT / "hooks" / "handback-inbox-poll.sh"

    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def poll(self, raw: str) -> subprocess.CompletedProcess:
        env = dict(os.environ, SHIPYARD_INBOX_DIR=str(self.dir))
        return subprocess.run(
            ["sh", str(self.POLL)],
            input=raw,
            capture_output=True,
            text=True,
            env=env,
            timeout=30,
            check=False,
        )

    def tool_payload(self, session: str = SESSION, size: int = 0) -> str:
        return json.dumps(
            {
                "session_id": session,
                "hook_event_name": "PostToolUse",
                "tool_name": "Bash",
                "tool_response": {"stdout": "y" * size},
            }
        )

    def test_no_inbox_prints_nothing(self) -> None:
        result = self.poll(self.tool_payload())
        self.assertEqual((result.returncode, result.stdout), (0, ""))

    def test_an_empty_inbox_starts_no_interpreter(self) -> None:
        # The poll runs on every tool call: with nothing waiting it must not
        # start Python at all.
        bin_dir = self.dir / "bin"
        bin_dir.mkdir()
        marker = self.dir / "python-started"
        fake = bin_dir / "python3"
        fake.write_text(f"#!/bin/sh\ntouch {marker}\n", encoding="utf-8")
        fake.chmod(0o755)
        env = dict(
            os.environ,
            SHIPYARD_INBOX_DIR=str(self.dir),
            PATH=f"{bin_dir}:/usr/bin:/bin",
        )
        for inbox in (None, ""):
            if inbox is not None:
                (self.dir / f"{SESSION}.jsonl").write_text(inbox, encoding="utf-8")
            subprocess.run(
                ["sh", str(self.POLL)],
                input=self.tool_payload(),
                capture_output=True,
                text=True,
                env=env,
                timeout=30,
                check=False,
            )
            self.assertFalse(marker.exists(), f"python started for inbox={inbox!r}")
        # Control: with an entry waiting, it does start.
        (self.dir / f"{SESSION}.jsonl").write_text(entry(9004) + "\n", encoding="utf-8")
        subprocess.run(
            ["sh", str(self.POLL)],
            input=self.tool_payload(),
            capture_output=True,
            text=True,
            env=env,
            timeout=30,
            check=False,
        )
        self.assertTrue(marker.exists())

    def test_a_waiting_entry_reaches_a_busy_session_after_one_tool_call(self) -> None:
        (self.dir / f"{SESSION}.jsonl").write_text(entry(9002) + "\n", encoding="utf-8")
        result = self.poll(self.tool_payload(size=2_000_000))
        self.assertEqual(result.returncode, 0)
        output = json.loads(result.stdout)
        self.assertEqual(output["hookSpecificOutput"]["hookEventName"], "PostToolUse")
        self.assertIn("#9002", output["hookSpecificOutput"]["additionalContext"])
        self.assertTrue((self.dir / f"{SESSION}.shown.jsonl").exists())
        # Shown once: the next tool call is silent again.
        again = self.poll(self.tool_payload())
        self.assertEqual(again.stdout, "")

    def test_another_sessions_inbox_and_bad_input_stay_silent(self) -> None:
        (self.dir / f"{SESSION}.jsonl").write_text(entry(9003) + "\n", encoding="utf-8")
        other = "0d9a2b6c-0000-4000-8000-000000000000"
        for raw in (self.tool_payload(session=other), "not json", "", '{"session_id": "../x"}'):
            result = self.poll(raw)
            self.assertEqual((result.returncode, result.stdout), (0, ""), raw[:40])
        self.assertTrue((self.dir / f"{SESSION}.jsonl").exists())

    def test_plugin_registers_the_poll_for_every_tool(self) -> None:
        hooks = json.loads((ROOT / "hooks" / "hooks.json").read_text(encoding="utf-8"))["hooks"]
        commands = [
            (group.get("matcher"), hook["command"])
            for group in hooks["PostToolUse"]
            for hook in group["hooks"]
        ]
        self.assertIn(("*", "sh ${CLAUDE_PLUGIN_ROOT}/hooks/handback-inbox-poll.sh"), commands)


if __name__ == "__main__":
    unittest.main()

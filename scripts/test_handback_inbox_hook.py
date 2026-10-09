#!/usr/bin/env python3
"""Tests for hooks/handback-inbox.py (the PR hand-back inbox reader)."""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
HOOK = ROOT / "hooks" / "handback-inbox.py"
SESSION = "cfc73f94-128a-4c3d-8e69-2f278ff4fd8b"


def entry(pr: int, episode: str = "2026-09-29T01:00:00Z", **extra: object) -> str:
    return json.dumps(
        {
            **extra,
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

    def test_each_note_names_its_head_and_age(self) -> None:
        stamp = (datetime.now(timezone.utc) - timedelta(hours=3, minutes=5)).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        )
        (self.dir / f"{SESSION}.jsonl").write_text(
            entry(9060, head_sha="0f6b0d273901aa", delivered_at=stamp) + "\n",
            encoding="utf-8",
        )
        result = self.run_hook(self.payload())
        context = json.loads(result.stdout)["hookSpecificOutput"]["additionalContext"]
        self.assertIn("on head 0f6b0d2, 3h ago", context)
        self.assertIn("if you pushed after that head", context)

    def test_a_retraction_drops_the_unread_note_it_names(self) -> None:
        stale = json.loads(entry(9060))["id"]
        retraction = json.dumps(
            {
                "schema": "shipyard.pr-watch.handback/v1",
                "retract": stale,
                "pr": 9060,
                "reason": "addressed",
            }
        )
        inbox = self.dir / f"{SESSION}.jsonl"
        inbox.write_text(entry(9060) + "\n" + retraction + "\n", encoding="utf-8")
        self.assertEqual(self.run_hook(self.payload()).stdout, "")
        self.assertFalse(inbox.exists())
        # A note that was not retracted still shows alongside a retraction.
        inbox.write_text(
            entry(9060, episode="2026-09-30T01:00:00Z") + "\n" + entry(9055) + "\n"
            + json.dumps({"retract": json.loads(entry(9055))["id"]}) + "\n",
            encoding="utf-8",
        )
        context = json.loads(self.run_hook(self.payload()).stdout)["hookSpecificOutput"][
            "additionalContext"
        ]
        self.assertIn("#9060", context)
        self.assertNotIn("#9055", context)

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


if __name__ == "__main__":
    unittest.main()

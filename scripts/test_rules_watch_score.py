"""Tests for rules_watch_score.py on a small labelled set."""

from __future__ import annotations

import csv
import importlib.util
import json
import pathlib
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).with_name("rules_watch_score.py")
SPEC = importlib.util.spec_from_file_location("rules_watch_score", SCRIPT)
assert SPEC and SPEC.loader
score_mod = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(score_mod)

FIELDS = ["turn", "ts", "trigger", "actionable", "kinds", "detectable",
          "prompt_excerpt", "final_text_excerpt"]


def turn(number: int, ts: str, actionable: str, kinds: str, detectable: str,
         text: str = "") -> dict[str, str]:
    return {"turn": str(number), "ts": ts, "trigger": "close_monitor",
            "actionable": actionable, "kinds": kinds, "detectable": detectable,
            "prompt_excerpt": "", "final_text_excerpt": text}


def event(kind: str, at: str, pr: int | None = None) -> dict:
    line = {"kind": kind, "key": f"{kind}:{at}:{pr}", "at": at}
    if pr is not None:
        line["pr"] = pr
    return line


class Score(unittest.TestCase):
    def run_score(self, rows: list[dict[str, str]], events: list[dict]) -> dict:
        with tempfile.TemporaryDirectory() as directory:
            turns_path = pathlib.Path(directory) / "turns.csv"
            with turns_path.open("w", newline="", encoding="utf-8") as handle:
                writer = csv.DictWriter(handle, fieldnames=FIELDS)
                writer.writeheader()
                writer.writerows(rows)
                writer.writerow({**turn(99, "2026-10-11T00:00:00Z", "b", "route_new_head",
                                        "rule"), "trigger": "teammate"})
            events_path = pathlib.Path(directory) / "events.jsonl"
            events_path.write_text("".join(json.dumps(e) + "\n" for e in events),
                                   encoding="utf-8")
            return score_mod.score(
                score_mod.load_turns(str(turns_path), "close_monitor"),
                score_mod.load_events(str(events_path)),
                [10, 30],
            )

    def test_recall_needs_a_mapped_kind_inside_the_window(self) -> None:
        rows = [
            turn(1, "2026-10-11T00:10:00Z", "b", "route_new_head", "rule", "routed #10156"),
            turn(2, "2026-10-11T00:20:00Z", "b", "red_or_blocked_check;spawned_agent",
                 "rule_detect_judgement_fix"),
            turn(3, "2026-10-11T00:30:00Z", "b", "spawned_agent", "judgement"),
            turn(4, "2026-10-11T00:40:00Z", "b", "codex_session_resume_or_flush", "rule"),
        ]
        events = [
            event("ready_unreviewed", "2026-10-11T00:05:00Z", 10156),
            # The wrong kind for turn 2, and the right kind too early for 10 minutes.
            event("ready_unreviewed", "2026-10-11T00:18:00Z", 1),
            event("required_red", "2026-10-11T00:05:00Z", 2),
        ]
        report = self.run_score(rows, events)
        self.assertEqual(report["turns"], 4, "only close-monitor turns")
        self.assertEqual(report["rule_detectable"], 3)
        ten = report["windows"]["10"]["kind_time"]
        self.assertEqual(ten["recalled"], 1)
        self.assertEqual(ten["per_kind"]["red_or_blocked_check"]["recalled"], 0)
        thirty = report["windows"]["30"]["kind_time"]
        self.assertEqual(thirty["recalled"], 2)
        self.assertEqual(thirty["per_kind"]["codex_session_resume_or_flush"],
                         {"recalled": 0, "of": 1, "recall": 0.0})
        self.assertIn("codex_session_resume_or_flush", report["unsourced_kinds"])

    def test_the_pr_variant_requires_a_named_pull_request(self) -> None:
        rows = [turn(1, "2026-10-11T00:10:00Z", "b", "route_new_head", "rule", "routed #10156")]
        wrong = self.run_score(rows, [event("ready_unreviewed", "2026-10-11T00:05:00Z", 9999)])
        self.assertEqual(wrong["windows"]["10"]["kind_time"]["recalled"], 1)
        self.assertEqual(wrong["windows"]["10"]["kind_time_pr"]["recalled"], 0)
        right = self.run_score(rows, [event("ready_unreviewed", "2026-10-11T00:05:00Z", 10156)])
        self.assertEqual(right["windows"]["10"]["kind_time_pr"]["recalled"], 1)
        self.assertEqual(right["windows"]["10"]["kind_time_pr_strict"]["recalled"], 1)

    def test_the_strict_variant_never_matches_an_event_without_a_pull_request(self) -> None:
        rows = [turn(1, "2026-10-11T00:10:00Z", "b", "host_down_or_out_of_pool", "rule", "m5s off")]
        report = self.run_score(rows, [event("host_alarm", "2026-10-11T00:05:00Z")])
        self.assertEqual(report["windows"]["10"]["kind_time_pr"]["recalled"], 1)
        self.assertEqual(report["windows"]["10"]["kind_time_pr_strict"]["recalled"], 0)

    def test_a_false_wake_is_any_event_since_the_previous_turn(self) -> None:
        rows = [
            turn(1, "2026-10-11T00:10:00Z", "b", "route_new_head", "rule"),
            turn(2, "2026-10-11T00:20:00Z", "a", "", "n/a"),
            turn(3, "2026-10-11T00:30:00Z", "a", "", "n/a"),
        ]
        events = [
            event("verdict_posted", "2026-10-11T00:10:00Z"),  # belongs to turn 1
            event("host_alarm", "2026-10-11T00:25:00Z"),       # wakes turn 3
        ]
        report = self.run_score(rows, events)
        self.assertEqual(report["nothing_actionable"], 2)
        self.assertEqual(report["windows"]["10"]["false_wakes"], 1)
        self.assertEqual(report["windows"]["10"]["false_wake_rate"], 0.5)


    def test_sourced_recall_leaves_out_turns_no_stream_can_see(self) -> None:
        rows = [
            turn(1, "2026-10-11T00:10:00Z", "b", "route_new_head", "rule"),
            turn(2, "2026-10-11T00:20:00Z", "b", "codex_session_resume_or_flush", "rule"),
        ]
        report = self.run_score(rows, [event("ready_unreviewed", "2026-10-11T00:05:00Z", 1)])
        data = report["windows"]["10"]["kind_time"]
        self.assertEqual(data["recall"], 0.5)
        self.assertEqual(data["sourced"], {"turns": 1, "recalled": 1, "recall": 1.0})

    def test_a_shift_moves_events_around_the_span(self) -> None:
        events = score_mod.load_events.__globals__["parse_time"]
        stream = [
            {"kind": "x", "_at": events("2026-10-11T00:00:00Z")},
            {"kind": "y", "_at": events("2026-10-11T10:00:00Z")},
        ]
        moved = score_mod.shifted(stream, 6)
        self.assertEqual([e["kind"] for e in moved], ["y", "x"])
        self.assertEqual(moved[1]["_at"], events("2026-10-11T06:00:00Z"))


    def test_host_and_codex_reader_kinds_map_to_their_turn_kinds(self) -> None:
        rows = [
            turn(1, "2026-10-11T00:10:00Z", "b", "host_down_or_out_of_pool", "rule"),
            turn(2, "2026-10-11T01:10:00Z", "b", "codex_session_resume_or_flush", "rule"),
        ]
        events = [event("host_not_serving", "2026-10-11T00:05:00Z"),
                  event("codex_session_dead", "2026-10-11T01:05:00Z")]
        report = self.run_score(rows, events)
        self.assertEqual(report["windows"]["10"]["kind_time"]["recalled"], 2)
        self.assertNotIn("host_down_or_out_of_pool", report["unsourced_kinds"])
        self.assertNotIn("codex_session_resume_or_flush", report["unsourced_kinds"])


if __name__ == "__main__":
    unittest.main()

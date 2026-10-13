#!/usr/bin/env python3
"""Score a `shipyard pr-watch rules-replay` event stream against labelled turns.

The labelled turns are a close monitor's periodic checks, one CSV row each
(`ts`, `trigger`, `actionable`, `kinds`, `detectable`, `prompt_excerpt`,
`final_text_excerpt`), as in the fleet friction audit's
`team-lead-turn-labels.csv`. A turn is *rule-detectable* when `detectable` is
`rule` or `rule_detect_judgement_fix`; it is *nothing actionable* when
`actionable` is `a`.

For each rule-detectable turn the watcher **recalls** it when, within the
window before the turn, it emitted an event of a kind that maps to one of the
turn's kinds (KIND_MAP). The `pr` variant also requires the event's pull
request to be one the turn names, when the turn names any; the `pr_strict`
variant counts only events that name a pull request the turn names, so an
event with no pull request (a host alarm) never matches. A
nothing-actionable turn is a **false wake** when the watcher emitted any event
between the previous turn and this one: the watcher would have woken a model
that then found nothing to do.

Kinds with no source in the event stream (a Codex tab's state comes from
cmux, not GitHub) count as misses unless an input-event stream supplied them,
and are reported separately so the gap is visible rather than hidden;
`sourced` repeats the recall over the turns that have at least one sourced
kind.

A persistent condition (a host out of the pool, a dead Codex session) is
re-reported turn after turn while the watcher fires once when it begins. The
`kind_episode` variant scores that fairly: consecutive turns of a persistent
kind no more than `--episode-gap-hours` apart form one episode, and a turn is
recalled when a matching event fired within the window before the episode's
first turn or at any time since, up to this turn (for other kinds it is
`kind_time`). It also reports how many episodes were recalled at their
opening turn.

A recall measured on time alone rises with event density: a stream that emits
often enough matches any turn. `--control-shift-hours N` scores the same
events shifted N hours around the window as a negative control; a recall that
does not fall under the shift is density, not detection.
"""

from __future__ import annotations

import argparse
import csv
import json
import re
import sys
from collections import Counter, defaultdict
from datetime import datetime, timedelta, timezone
from typing import Any

# Turn kind -> watcher event kinds that would have woken the model for it.
KIND_MAP: dict[str, tuple[str, ...]] = {
    "route_new_head": ("ready_unreviewed", "head_unreviewed", "verdict_posted"),
    "unreviewed_in_queue": ("armed_unapproved",),
    "dequeue_or_disarm": ("armed_unapproved",),
    "red_or_blocked_check": (
        "required_red", "group_red", "queue_removed", "queue_stuck", "approved_blocked",
    ),
    "merge_or_arm": ("approved_unarmed",),
    "host_down_or_out_of_pool": (
        "host_alarm", "host_not_serving", "host_problem", "host_out_of_pool", "host_unreachable",
    ),
    "codex_session_resume_or_flush": (
        "codex_session_dead", "codex_session_errored", "codex_draft_unsent", "codex_draft_queued",
    ),
}
RULE_LABELS = ("rule", "rule_detect_judgement_fix")
# Conditions that persist until fixed: the watcher reports each once, when it
# begins, while a monitor re-reports it every turn.
PERSISTENT_KINDS = ("host_down_or_out_of_pool", "codex_session_resume_or_flush")
VARIANTS = ("kind_time", "kind_time_pr", "kind_time_pr_strict", "kind_episode")
PR_REF = re.compile(r"#(\d{3,5})\b")


def parse_time(text: str) -> datetime:
    return datetime.fromisoformat(text.replace("Z", "+00:00")).astimezone(timezone.utc)


def load_turns(path: str, trigger: str) -> list[dict[str, Any]]:
    with open(path, newline="", encoding="utf-8") as handle:
        rows = [row for row in csv.DictReader(handle) if row.get("trigger") == trigger]
    turns = []
    for row in rows:
        text = f"{row.get('prompt_excerpt', '')} {row.get('final_text_excerpt', '')}"
        turns.append({
            "turn": row.get("turn"),
            "ts": parse_time(row["ts"]),
            "actionable": row.get("actionable") == "b",
            "rule": row.get("detectable") in RULE_LABELS,
            "kinds": [kind for kind in (row.get("kinds") or "").split(";") if kind],
            "prs": {int(number) for number in PR_REF.findall(text)},
        })
    turns.sort(key=lambda turn: turn["ts"])
    return turns


def load_events(path: str) -> list[dict[str, Any]]:
    events = []
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            if line.strip():
                event = json.loads(line)
                event["_at"] = parse_time(event["at"])
                events.append(event)
    events.sort(key=lambda event: event["_at"])
    return events


def matches(event: dict[str, Any], kind: str, turn: dict[str, Any], window: timedelta,
            by_pr: str | bool) -> bool:
    if event["kind"] not in KIND_MAP.get(kind, ()):
        return False
    if not turn["ts"] - window < event["_at"] <= turn["ts"]:
        return False
    if by_pr == "strict":
        return event.get("pr") is not None and event["pr"] in turn["prs"]
    if by_pr and turn["prs"] and event.get("pr") is not None:
        return event["pr"] in turn["prs"]
    return True


def episode_starts(turns: list[dict[str, Any]], gap: timedelta
                   ) -> dict[tuple[int, str], datetime]:
    """``(turn index, persistent kind) -> its episode's first turn time``.

    Consecutive rule-detectable turns naming the same persistent kind no more
    than ``gap`` apart are one episode.
    """
    starts: dict[tuple[int, str], datetime] = {}
    for kind in PERSISTENT_KINDS:
        start = previous = None
        for index, turn in enumerate(turns):
            if not turn["rule"] or kind not in turn["kinds"]:
                continue
            if previous is None or turn["ts"] - previous > gap:
                start = turn["ts"]
            previous = turn["ts"]
            starts[(index, kind)] = start
    return starts


def score(turns: list[dict[str, Any]], events: list[dict[str, Any]],
          windows: list[int], episode_gap: timedelta = timedelta(hours=2)) -> dict[str, Any]:
    sourced = {event["kind"] for event in events}
    report: dict[str, Any] = {
        "turns": len(turns),
        "rule_detectable": sum(turn["rule"] for turn in turns),
        "nothing_actionable": sum(not turn["actionable"] for turn in turns),
        "events": len(events),
        "events_by_kind": dict(Counter(event["kind"] for event in events)),
        "unsourced_kinds": sorted(
            kind for kind, wanted in KIND_MAP.items() if not sourced.intersection(wanted)
        ),
        "windows": {},
    }
    if turns:
        hours = max((turns[-1]["ts"] - turns[0]["ts"]).total_seconds() / 3600, 1e-9)
        inside = [e for e in events if turns[0]["ts"] - timedelta(minutes=10) < e["_at"] <= turns[-1]["ts"]]
        report["events_per_hour"] = round(len(inside) / hours, 2)
    starts = episode_starts(turns, episode_gap)
    for minutes in windows:
        window = timedelta(minutes=minutes)
        entry: dict[str, Any] = {}
        for variant, by_pr in (("kind_time", False), ("kind_time_pr", True),
                               ("kind_time_pr_strict", "strict"), ("kind_episode", False)):
            recalled = 0
            sourced_turns = 0
            sourced_recalled = 0
            per_kind: dict[str, list[int]] = defaultdict(lambda: [0, 0])
            episodes: dict[str, list[int]] = defaultdict(lambda: [0, 0])
            for index, turn in enumerate(turns):
                if not turn["rule"]:
                    continue
                hit = False
                for kind in turn["kinds"]:
                    if kind not in KIND_MAP:
                        continue
                    start = starts.get((index, kind)) if variant == "kind_episode" else None
                    if start is not None:
                        # The episode is open from its first turn: an event
                        # that fired as it began recalls every turn in it.
                        reach = turn["ts"] - start + window
                        found = any(matches(e, kind, turn, reach, False) for e in events)
                        if start == turn["ts"]:
                            episodes[kind][1] += 1
                            episodes[kind][0] += found
                    else:
                        found = any(matches(e, kind, turn, window, by_pr) for e in events)
                    per_kind[kind][1] += 1
                    per_kind[kind][0] += found
                    hit |= found
                recalled += hit
                if any(sourced.intersection(KIND_MAP.get(kind, ())) for kind in turn["kinds"]):
                    sourced_turns += 1
                    sourced_recalled += hit
            total = report["rule_detectable"]
            entry[variant] = {
                "recalled": recalled,
                "recall": round(recalled / total, 3) if total else None,
                "sourced": {
                    "turns": sourced_turns,
                    "recalled": sourced_recalled,
                    "recall": round(sourced_recalled / sourced_turns, 3) if sourced_turns else None,
                },
                "per_kind": {
                    kind: {"recalled": hits, "of": of, "recall": round(hits / of, 3)}
                    for kind, (hits, of) in sorted(per_kind.items())
                },
            }
            if variant == "kind_episode":
                entry[variant]["episodes"] = {
                    kind: {"recalled": hits, "of": of, "recall": round(hits / of, 3)}
                    for kind, (hits, of) in sorted(episodes.items())
                }
        false_wakes = 0
        previous = None
        for turn in turns:
            start = previous if previous is not None else turn["ts"] - timedelta(minutes=10)
            previous = turn["ts"]
            if turn["actionable"]:
                continue
            if any(start < event["_at"] <= turn["ts"] for event in events):
                false_wakes += 1
        quiet = report["nothing_actionable"]
        entry["false_wakes"] = false_wakes
        entry["false_wake_rate"] = round(false_wakes / quiet, 3) if quiet else None
        report["windows"][str(minutes)] = entry
    return report


def render(report: dict[str, Any]) -> str:
    lines = [
        f"turns {report['turns']}, rule-detectable {report['rule_detectable']}, "
        f"nothing actionable {report['nothing_actionable']}, events {report['events']}"
        f" ({report.get('events_per_hour', 0)}/h)",
        f"unsourced kinds (counted as misses): {', '.join(report['unsourced_kinds']) or 'none'}",
    ]
    for minutes, entry in report["windows"].items():
        for variant in VARIANTS:
            data = entry[variant]
            sourced = data["sourced"]
            lines.append(f"window {minutes}m {variant}: recall {data['recall']} "
                         f"({data['recalled']}/{report['rule_detectable']}); over sourced "
                         f"turns {sourced['recall']} ({sourced['recalled']}/{sourced['turns']})")
            for kind, numbers in data["per_kind"].items():
                lines.append(f"  {kind}: {numbers['recalled']}/{numbers['of']} ({numbers['recall']})")
            for kind, numbers in data.get("episodes", {}).items():
                lines.append(f"  {kind} episodes recalled at their opening turn: "
                             f"{numbers['recalled']}/{numbers['of']} ({numbers['recall']})")
        lines.append(f"window {minutes}m false wakes: {entry['false_wakes']}/"
                     f"{report['nothing_actionable']} ({entry['false_wake_rate']})")
    control = report.get("control")
    if control:
        for minutes, entry in control["windows"].items():
            lines.append(
                f"control (events shifted {control['shift_hours']} h) window {minutes}m: "
                f"recall kind_time {entry['kind_time']['recall']}, "
                f"kind_time_pr {entry['kind_time_pr']['recall']}, "
                f"kind_time_pr_strict {entry['kind_time_pr_strict']['recall']}, "
                f"kind_episode {entry['kind_episode']['recall']} (host turns "
                f"{entry['kind_episode']['per_kind'].get('host_down_or_out_of_pool', {}).get('recalled')}), "
                f"false wakes {entry['false_wake_rate']}")
    return "\n".join(lines) + "\n"


def shifted(events: list[dict[str, Any]], hours: float) -> list[dict[str, Any]]:
    """Every event moved `hours` later, wrapping around the stream's span."""
    if not events:
        return []
    start = min(event["_at"] for event in events)
    span = max(event["_at"] for event in events) - start + timedelta(seconds=1)
    shift = timedelta(hours=hours)
    moved = [{**event, "_at": start + (event["_at"] - start + shift) % span} for event in events]
    moved.sort(key=lambda event: event["_at"])
    return moved


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--turns", required=True, help="labelled turns CSV")
    parser.add_argument("--events", required=True, help="rules-replay JSONL")
    parser.add_argument("--trigger", default="close_monitor")
    parser.add_argument("--windows", default="10,30,60", help="minutes, comma-separated")
    parser.add_argument("--control-shift-hours", type=float, default=None,
                        help="also score the events shifted this many hours (negative control)")
    parser.add_argument("--episode-gap-hours", type=float, default=2.0,
                        help="turns of a persistent kind this close are one episode")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    windows = [int(part) for part in args.windows.split(",") if part.strip()]
    turns = load_turns(args.turns, args.trigger)
    events = load_events(args.events)
    gap = timedelta(hours=args.episode_gap_hours)
    report = score(turns, events, windows, gap)
    if args.control_shift_hours is not None:
        control = score(turns, shifted(events, args.control_shift_hours), windows, gap)
        report["control"] = {"shift_hours": args.control_shift_hours,
                             "windows": control["windows"]}
    if args.json:
        json.dump(report, sys.stdout, indent=2, sort_keys=True)
        sys.stdout.write("\n")
    else:
        sys.stdout.write(render(report))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

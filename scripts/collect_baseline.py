#!/usr/bin/env python3
"""Collect organic Shipyard baseline telemetry and emit an honest scorecard.

The wrapper delegates scorecard semantics to Shipyard's Rust metrics command.
It never inserts rows, classifies strata with guessed substrings, or infers a
safe merge from a pull request.
"""
from __future__ import annotations

import argparse
import datetime as dt
import json
import pathlib
import os
import subprocess
import sqlite3
import tempfile
import sys
from typing import Any, Sequence

REQUIRED_STRATA = ("normal_pr", "merge_group", "rerun", "outage", "concurrent_agents")


def run(command: Sequence[str], *, stdin: str | None = None, timeout: int = 300) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(command, input=stdin, text=True, capture_output=True, check=False, timeout=timeout)
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(f"command timed out after {timeout}s: {command[0]}") from exc


def import_organic(args: argparse.Namespace) -> list[dict[str, Any]]:
    events: list[dict[str, Any]] = []
    state_dir = str(validate_state_layout(args.db))
    before = count_jobs(args.db, args.project, args.repo)
    github = run([args.shipyard, "--state-dir", state_dir, "metrics", "import", "github", "--repo", args.repo,
                  "--project", args.project, "--limit", str(args.limit), "--json"], timeout=args.timeout_seconds)
    if github.returncode:
        raise RuntimeError(f"GitHub import failed: {github.stderr.strip()}")
    events.append(summarize_import("github", github.stdout))
    tartci = run([args.tartci, "runtime", "export", "--repo", args.repo], timeout=args.timeout_seconds)
    validate_tartci_export(tartci.stdout, args.repo)
    if tartci.returncode:
        raise RuntimeError(f"TartCI export failed: {tartci.stderr.strip()}")
    imported = run([args.shipyard, "--state-dir", state_dir, "metrics", "import", "tartci", "--json"], stdin=tartci.stdout, timeout=args.timeout_seconds)
    if imported.returncode:
        raise RuntimeError(f"TartCI import failed: {imported.stderr.strip()}")
    events.append(summarize_import("tartci", imported.stdout))
    after = count_jobs(args.db, args.project, args.repo)
    if before is not None and after is not None:
        events.append({"source": "collection", "status": "completed", "new_rows": max(after - before, 0)})
    else:
        events.append({"source": "collection", "status": "unavailable", "new_rows": None})
    return events

def count_jobs(db: pathlib.Path, project: str, repo: str) -> int | None:
    try:
        with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as connection:
            return int(connection.execute("""select count(*) from jobs j join runs r on r.id=j.run_id
                where r.project=? and (r.repo=? or r.repo is null)""", (project, repo)).fetchone()[0])
    except sqlite3.Error:
        return None


def validate_tartci_export(text: str, repo: str) -> None:
    if not text.strip():
        raise RuntimeError("TartCI export was empty")
    records = []
    try:
        value = json.loads(text)
        records = value if isinstance(value, list) else [value]
    except json.JSONDecodeError:
        for line in text.splitlines():
            if line.strip():
                try:
                    records.append(json.loads(line))
                except json.JSONDecodeError as exc:
                    raise RuntimeError("TartCI export contained malformed JSON") from exc
    if not records or any(not isinstance(record, dict) for record in records):
        raise RuntimeError("TartCI export must contain JSON objects")
    mismatched = [record.get("repo") for record in records if record.get("repo") not in (None, repo)]
    if mismatched:
        raise RuntimeError(f"TartCI export contains another repository: {mismatched[0]}")


def summarize_import(source: str, output: str) -> dict[str, Any]:
    try:
        value = json.loads(output)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"{source} import returned malformed JSON") from exc
    if not isinstance(value, dict):
        raise RuntimeError(f"{source} import returned non-object JSON")
    return {"source": source, "status": "completed", "imported": value.get("imported")}


def validate_state_layout(db: pathlib.Path) -> pathlib.Path:
    db = db.expanduser()
    if db.name != "metrics.db" or db.parent.name != "metrics":
        raise ValueError("--db must be <state-dir>/metrics/metrics.db")
    state_dir = db.parent.parent
    if state_dir / "metrics" / "metrics.db" != db:
        raise ValueError("--db must resolve to the selected state directory")
    return state_dir


def authoritative_scorecard(args: argparse.Namespace) -> dict[str, Any]:
    state_dir = validate_state_layout(args.db)
    result = run([args.shipyard, "--state-dir", str(state_dir), "metrics", "scorecard",
                  "--project", args.project, "--since", f"{args.since_days}d", "--json"], timeout=args.timeout_seconds)
    if result.returncode:
        raise RuntimeError(f"authoritative scorecard failed: {result.stderr.strip()}")
    try:
        card = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"authoritative scorecard was not JSON: {exc}") from exc
    # Keep the denominator explicit even when an older binary emits no field.
    card["collector_safe_merge_denominator"] = {
        "status": "unavailable", "value": None,
        "reason": "collector does not import or authenticate merge receipts",
    }
    card["provenance_status"] = "unverified_organic_input"
    card.pop("database", None)
    card["strata"] = {name: {"status": "unavailable", "reason":
                       "no authenticated stratum label is present in the imported telemetry"}
                      for name in REQUIRED_STRATA}
    card["missing_strata"] = list(REQUIRED_STRATA)
    return card


def update_state(path: pathlib.Path, project: str, started: dt.datetime,
                 card: dict[str, Any], events: list[dict[str, Any]],
                 observed_at: dt.datetime | None = None) -> dict[str, Any]:
    state = json.loads(path.read_text()) if path.exists() else {}
    state.setdefault("started_at", started.isoformat())
    state.setdefault("observations", [])
    observed_at = observed_at or dt.datetime.now(dt.timezone.utc)
    state["project"] = project
    state["last_observed_at"] = observed_at.isoformat()
    imported = any((event.get("new_rows") or 0) > 0 for event in events)
    if imported:
        state["observations"].append({"at": state["last_observed_at"], "events": events, "scorecard": card})
    observed_days = {item["at"][:10] for item in state["observations"]
                     if dt.date.fromisoformat(item["at"][:10]).weekday() < 5}
    state["weekday_days_observed"] = len(observed_days)
    state["baseline_exit"] = len(observed_days) >= 5 and not card.get("missing_strata")
    payload = json.dumps(state, indent=2, sort_keys=True) + "\n"
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile("w", dir=path.parent, prefix=path.name + ".", delete=False) as temporary:
        temporary.write(payload)
        temporary.flush()
        os.fsync(temporary.fileno())
        temporary_path = temporary.name
    os.replace(temporary_path, path)
    return state


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", type=pathlib.Path, required=True)
    parser.add_argument("--project", required=True)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--state", type=pathlib.Path, required=True)
    parser.add_argument("--status-markdown", type=pathlib.Path)
    parser.add_argument("--shipyard", default="shipyard")
    parser.add_argument("--tartci", default="tartci")
    parser.add_argument("--limit", type=int, default=100)
    parser.add_argument("--since-days", type=int, default=5)
    parser.add_argument("--collect", action="store_true", help="run provider imports")
    parser.add_argument("--timeout-seconds", type=int, default=300)
    args = parser.parse_args()
    if args.timeout_seconds <= 0:
        parser.error("--timeout-seconds must be positive")
    try:
        events = import_organic(args) if args.collect else []
        card = authoritative_scorecard(args)
        state = update_state(args.state, args.project, dt.datetime.now(dt.timezone.utc), card, events)
    except (OSError, RuntimeError, ValueError) as exc:
        print(f"baseline collection failed: {exc}", file=sys.stderr)
        return 1
    output = {"project": args.project, "database": "configured metrics store", "scorecard": card,
              "baseline": {"weekday_days_observed": state["weekday_days_observed"],
                           "exit": state["baseline_exit"]}, "events": events}
    if args.status_markdown:
        args.status_markdown.write_text(
            "# Organic baseline status\n\n"
            f"- Project: `{args.project}`\n- Observed: `{state['last_observed_at']}`\n"
            f"- Weekdays observed: **{state['weekday_days_observed']}** (exit requires five)\n"
            f"- Exit: **{'yes' if state['baseline_exit'] else 'no'}**\n"
            f"- Safely merged denominator: **{card.get('collector_safe_merge_denominator', {}).get('status', 'unavailable')}**\n"
            "- Provenance: **unverified organic input**; synthetic provenance is not asserted by this wrapper.\n"
            "- Strata: **unavailable** until authenticated labels are emitted by the producer.\n\n"
            "Synthetic provenance, unavailable phases, and merge denominators are not imputed.\n"
        )
    print(json.dumps(output, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

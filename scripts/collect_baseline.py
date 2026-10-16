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


def state_identity(path: pathlib.Path, project: str, repo: str, db: pathlib.Path) -> dict[str, str]:
    return {"project": project, "repo": repo, "database": str(db.expanduser().resolve(strict=False))}


def validate_state_identity(path: pathlib.Path, project: str, repo: str, db: pathlib.Path) -> None:
    if not path.exists():
        return
    try:
        state = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as exc:
        raise RuntimeError(f"baseline state is unreadable: {exc}") from exc
    expected = state_identity(path, project, repo, db)
    stored = state.get("identity")
    if stored is None:
        if state.get("observations"):
            raise RuntimeError("baseline state has observations but no identity; refusing cross-project reuse")
        return
    if any(stored.get(key) != value for key, value in expected.items()):
        raise RuntimeError(f"baseline state identity mismatch: stored={stored!r} requested={expected!r}")


def import_organic(args: argparse.Namespace) -> list[dict[str, Any]]:
    events: list[dict[str, Any]] = []
    validate_state_identity(args.state, args.project, args.repo, args.db)
    state_dir = str(validate_state_layout(args.db))
    before = count_jobs(args.db, args.project, args.repo)
    github = run([args.shipyard, "--state-dir", state_dir, "metrics", "import", "github", "--repo", args.repo,
                  "--project", args.project, "--limit", str(args.limit), "--json"], timeout=args.timeout_seconds)
    if github.returncode:
        raise RuntimeError(f"GitHub import failed: {github.stderr.strip()}")
    events.append(summarize_import("github", github.stdout))
    tartci = run([args.tartci, "runtime", "export", "--repo", args.repo, "--since-days", str(args.limit)], timeout=args.timeout_seconds)
    if tartci.returncode:
        raise RuntimeError(f"TartCI export failed: {tartci.stderr.strip()}")
    validate_tartci_export(tartci.stdout, args.repo)
    imported = run([args.shipyard, "--state-dir", state_dir, "metrics", "import", "tartci", "--json"], stdin=tartci.stdout, timeout=args.timeout_seconds)
    if imported.returncode:
        raise RuntimeError(f"TartCI import failed: {imported.stderr.strip()}")
    events.append(summarize_import("tartci", imported.stdout))
    after = count_jobs(args.db, args.project, args.repo)
    activity_dates = source_activity_dates(args.db, args.project, args.repo)
    if before is not None and after is not None:
        events.append({"source": "collection", "status": "completed", "new_rows": max(after - before, 0),
                       "source_activity_dates": sorted(activity_dates or []),
                       "source_activity_status": "available" if activity_dates is not None else "unavailable"})
    else:
        events.append({"source": "collection", "status": "unavailable", "new_rows": None,
                       "source_activity_dates": [], "source_activity_status": "unavailable"})
    return events

def count_jobs(db: pathlib.Path, project: str, repo: str) -> int | None:
    try:
        with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as connection:
            return int(connection.execute("""select count(*) from jobs j join runs r on r.id=j.run_id
                where r.project=? and r.repo=?""", (project, repo)).fetchone()[0])
    except sqlite3.Error:
        return None


def source_activity_dates(db: pathlib.Path, project: str, repo: str) -> set[str] | None:
    """Read provider job timestamps without using the run ingestion timestamp."""
    try:
        with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as connection:
            run_columns = {row[1] for row in connection.execute("pragma table_info(runs)")}
            job_columns = {row[1] for row in connection.execute("pragma table_info(jobs)")}
            timestamps = [name for name in ("completed_at", "started_at") if name in job_columns]
            if not timestamps or "project" not in run_columns or "repo" not in run_columns or "run_id" not in job_columns:
                return None
            timestamp_sql = "coalesce(" + ",".join("j." + name for name in timestamps) + ")"
            rows = connection.execute(
                f"select {timestamp_sql} from jobs j join runs r on r.id=j.run_id "
                "where r.project=? and r.repo=?", (project, repo))
            dates = set()
            for (value,) in rows:
                if isinstance(value, str):
                    try:
                        dates.add(dt.datetime.fromisoformat(value.replace("Z", "+00:00")).date().isoformat())
                    except ValueError:
                        continue
            return dates
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
    missing = [record for record in records if record.get("repo") != repo or not record.get("external_id")]
    if missing:
        raise RuntimeError("TartCI export record lacks exact repo or non-empty external_id")
    mismatched = [record.get("repo") for record in records if record.get("repo") != repo]
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
                  "--project", args.repo, "--since", f"{args.since_days}d", "--json"], timeout=args.timeout_seconds)
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
                 observed_at: dt.datetime | None = None, repo: str | None = None,
                 db: pathlib.Path | None = None) -> dict[str, Any]:
    state = json.loads(path.read_text()) if path.exists() else {}
    state.setdefault("started_at", started.isoformat())
    state.setdefault("observations", [])
    observed_at = observed_at or dt.datetime.now(dt.timezone.utc)
    if repo is not None and db is not None:
        validate_state_identity(path, project, repo, db)
        state["identity"] = state_identity(path, project, repo, db)
    state["project"] = project
    state["last_observed_at"] = observed_at.isoformat()
    imported = any((event.get("new_rows") or 0) > 0 for event in events)
    activity_dates = set()
    for event in events:
        activity_dates.update(value for value in event.get("source_activity_dates", []) if isinstance(value, str))
    baseline_start = dt.datetime.fromisoformat(state["started_at"].replace("Z", "+00:00")).date()
    if imported:
        state["observations"].append({"at": state["last_observed_at"], "events": events,
                                       "source_activity_dates": sorted(activity_dates), "scorecard": card})
    observed_days = {value for item in state["observations"] for value in item.get("source_activity_dates", [])
                     if value >= baseline_start.isoformat() and dt.date.fromisoformat(value).weekday() < 5}
    collection_days = {item["at"][:10] for item in state["observations"]
                       if dt.date.fromisoformat(item["at"][:10]).weekday() < 5}
    state["collection_weekday_days"] = len(collection_days)
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
        state = update_state(args.state, args.project, dt.datetime.now(dt.timezone.utc), card, events, repo=args.repo, db=args.db)
    except (OSError, RuntimeError, ValueError) as exc:
        print(f"baseline collection failed: {exc}", file=sys.stderr)
        return 1
    output = {"project": args.project, "database": "configured metrics store", "scorecard": card,
              "baseline": {"weekday_days_observed": state["weekday_days_observed"],
                           "collection_weekday_days": state.get("collection_weekday_days", 0),
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

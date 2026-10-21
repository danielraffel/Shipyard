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
import hashlib
import math
import pathlib
import os
import re
import subprocess
import sqlite3
import tempfile
import sys
from typing import Any, Sequence

from baseline_pass_history import (
    DEFAULT_RETENTION_WINDOW_SECS,
    HistoryValidationError,
    PassObservation,
    append_observation,
)

REQUIRED_STRATA = ("normal_pr", "merge_group", "rerun", "outage", "concurrent_agents")
STRATA_CONTRACT = "shipyard-metrics-strata/v1"
PRODUCER_STRATA = ("normal_pr", "merge_group", "rerun", "unclassified")


def run(command: Sequence[str], *, stdin: str | None = None, timeout: int = 300) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(command, input=stdin, text=True, capture_output=True, check=False, timeout=timeout)
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(f"command timed out after {timeout}s: {command[0]}") from exc


def state_identity(path: pathlib.Path, project: str, repo: str, db: pathlib.Path) -> dict[str, str]:
    return {"project": project, "repo": repo, "database": str(db.expanduser().resolve(strict=False))}


def validate_project_repo(project: str, repo: str) -> None:
    parts = repo.split("/")
    if len(parts) != 2 or not all(parts) or project.casefold() != parts[1].casefold():
        raise ValueError("--project must be the repository short name for exact scorecard scoping")


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
    validate_project_repo(args.project, args.repo)
    validate_state_identity(args.state, args.project, args.repo, args.db)
    state_dir = str(validate_state_layout(args.db))
    before = count_jobs(args.db, args.project, args.repo)
    github = run([args.shipyard, "--state-dir", state_dir, "metrics", "import", "github", "--repo", args.repo,
                  "--project", args.project, "--limit", str(args.limit), "--json"], timeout=args.timeout_seconds)
    if github.returncode:
        raise RuntimeError(f"GitHub import failed: {github.stderr.strip()}")
    events.append(summarize_import("github", github.stdout))
    tartci = run([args.tartci, "runtime", "export", "--repo", args.repo, "--since-days", str(args.since_days)], timeout=args.timeout_seconds)
    if tartci.returncode:
        raise RuntimeError(f"TartCI export failed: {tartci.stderr.strip()}")
    validate_tartci_export(tartci.stdout, args.repo, args.project)
    imported = run([args.shipyard, "--state-dir", state_dir, "metrics", "import", "tartci", "--json"], stdin=tartci.stdout, timeout=args.timeout_seconds)
    if imported.returncode:
        raise RuntimeError(f"TartCI import failed: {imported.stderr.strip()}")
    events.append(summarize_import("tartci", imported.stdout))
    unknown_imports = [event["source"] for event in events
                       if event.get("source") in {"github", "tartci"}
                       and (not isinstance(event.get("imported"), int)
                            or isinstance(event.get("imported"), bool)
                            or event.get("imported") < 0)]
    if unknown_imports:
        raise RuntimeError(
            "provider import count unavailable; refusing to record a successful pass: "
            + ", ".join(unknown_imports)
        )
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


def validate_tartci_export(text: str, repo: str, project: str) -> None:
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
    missing = [record for record in records if record.get("repo") != repo or record.get("project") != project
               or not isinstance(record.get("provider"), str) or not record["provider"].strip()
               or not isinstance(record.get("external_id"), str) or not record["external_id"].strip()]
    if missing:
        raise RuntimeError("TartCI export record lacks exact project/repo or non-empty provider/external_id")
    keys = [(record["provider"], record["external_id"]) for record in records]
    if len(keys) != len(set(keys)):
        raise RuntimeError("TartCI export contains duplicate provider/external_id records")


def summarize_import(source: str, output: str) -> dict[str, Any]:
    try:
        value = json.loads(output)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"{source} import returned malformed JSON") from exc
    if not isinstance(value, dict):
        raise RuntimeError(f"{source} import returned non-object JSON")
    return {"source": source, "status": "completed", "imported": value.get("imported")}


def consume_authenticated_strata(card: dict[str, Any], *, repo: str | None = None,
                                  since_days: int | None = None,
                                  reference_at: dt.datetime | None = None) -> dict[str, Any]:
    """Consume only the canonical authenticated job-observation receipt."""
    unavailable = {
        name: {"status": "unavailable", "reason":
               "no authenticated versioned stratum record is present"}
        for name in REQUIRED_STRATA
    }
    for name in ("outage", "concurrent_agents"):
        unavailable[name]["reason"] = "independent authenticated provider contract is not available"
    payload = card.get("strata")
    if not isinstance(payload, dict) or payload.get("schema") != STRATA_CONTRACT:
        card["strata"] = unavailable
        card["missing_strata"] = list(REQUIRED_STRATA)
        card["provenance_status"] = "unverified_organic_input"
        return card
    if repo is not None and payload.get("repo") != repo:
        card["strata"] = unavailable
        card["missing_strata"] = list(REQUIRED_STRATA)
        card["provenance_status"] = "unverified_organic_input"
        return card
    if payload.get("unit") != "job" or not isinstance(payload.get("source_head"), str) \
            or not re.fullmatch(r"[0-9a-fA-F]{40}", payload["source_head"]) \
            or not isinstance(payload.get("generated_at"), str):
        card["strata"] = unavailable
        card["missing_strata"] = list(REQUIRED_STRATA)
        card["provenance_status"] = "unverified_organic_input"
        return card
    try:
        window_since = dt.datetime.fromisoformat(str(payload["window_since"]).replace("Z", "+00:00"))
        window_until = dt.datetime.fromisoformat(str(payload["window_until"]).replace("Z", "+00:00"))
        generated_at = dt.datetime.fromisoformat(payload["generated_at"].replace("Z", "+00:00"))
        if any(value.tzinfo is None or value.utcoffset() is None
               for value in (window_since, window_until, generated_at)):
            raise ValueError("strata timestamps must include timezone offsets")
    except (KeyError, TypeError, ValueError):
        window_since = window_until = None
    reference_at = reference_at or dt.datetime.now(dt.timezone.utc)
    if reference_at.tzinfo is None or reference_at.utcoffset() is None:
        reference_at = None
    if window_since is None or window_until is None or reference_at is None \
            or window_until <= window_since \
            or (since_days is not None and (window_until - window_since).total_seconds()
                != since_days * 86400) \
            or (since_days is not None and abs((window_until - reference_at).total_seconds()) > 300) \
            or (since_days is not None and abs((window_since -
                (reference_at - dt.timedelta(days=since_days))).total_seconds()) > 300) \
            or generated_at < window_until \
            or generated_at > reference_at + dt.timedelta(seconds=300):
        card["strata"] = unavailable
        card["missing_strata"] = list(REQUIRED_STRATA)
        card["provenance_status"] = "unverified_organic_input"
        return card
    records = payload.get("strata")
    aggregate = payload.get("aggregate")
    if not isinstance(records, dict) or set(records) != set(PRODUCER_STRATA) \
            or not isinstance(aggregate, dict):
        card["strata"] = unavailable
        card["missing_strata"] = list(REQUIRED_STRATA)
        card["provenance_status"] = "unverified_organic_input"
        return card

    def valid_count(value: Any) -> bool:
        return isinstance(value, int) and not isinstance(value, bool) and value >= 0

    if not valid_count(aggregate.get("observed_jobs")) or not valid_count(aggregate.get("qualified_jobs")) \
            or aggregate["qualified_jobs"] > aggregate["observed_jobs"]:
        return {**card, "strata": unavailable, "missing_strata": list(REQUIRED_STRATA),
                "provenance_status": "unverified_organic_input"}
    observed_total = qualified_total = 0
    accepted: dict[str, dict[str, Any]] = {}
    sufficient: set[str] = set()
    for name in PRODUCER_STRATA:
        record = records.get(name)
        if not isinstance(record, dict):
            return {**card, "strata": unavailable, "missing_strata": list(REQUIRED_STRATA),
                    "provenance_status": "unverified_organic_input"}
        observed = record.get("observed_jobs")
        qualified = record.get("qualified_jobs")
        if not valid_count(observed) or not valid_count(qualified) or qualified > observed \
                or record.get("provenance") not in {"authenticated", "unverified", "unknown"} \
                or record.get("coverage") not in {"complete", "partial", "unavailable"} \
                or not isinstance(record.get("reason"), str) \
                or (record["provenance"] != "authenticated" and qualified != 0):
            return {**card, "strata": unavailable, "missing_strata": list(REQUIRED_STRATA),
                    "provenance_status": "unverified_organic_input"}
        observed_total += observed
        qualified_total += qualified
        if record["provenance"] == "authenticated" and record["coverage"] == "complete":
            accepted[name] = dict(record)
            # Mandatory category presence is distinct from a producer-selected
            # sample threshold: a measured zero remains available but cannot
            # establish representative activity.
            if qualified > 0:
                sufficient.add(name)
    if aggregate["observed_jobs"] != observed_total or aggregate["qualified_jobs"] != qualified_total:
        return {**card, "strata": unavailable, "missing_strata": list(REQUIRED_STRATA),
                "provenance_status": "unverified_organic_input"}

    # Preserve unverified/unknown observations for audit visibility while only
    # authenticated complete records can enter the promotion population.
    card["strata"] = {name: dict(records[name]) for name in PRODUCER_STRATA}
    for name in ("outage", "concurrent_agents"):
        card["strata"][name] = unavailable[name]
    card["strata_sufficiency"] = {
        name: {"status": "sufficient" if name in sufficient else "insufficient",
               "qualified_jobs": card["strata"].get(name, {}).get("qualified_jobs")}
        for name in REQUIRED_STRATA
    }
    card["missing_strata"] = [name for name in REQUIRED_STRATA if name not in sufficient]
    card["provenance_status"] = (
        "authenticated_strata_complete" if len(sufficient) == len(REQUIRED_STRATA)
        else "authenticated_strata_partial" if accepted else "unverified_organic_input"
    )
    return card


def validate_state_layout(db: pathlib.Path) -> pathlib.Path:
    db = db.expanduser()
    if db.name != "metrics.db" or db.parent.name != "metrics":
        raise ValueError("--db must be <state-dir>/metrics/metrics.db")
    state_dir = db.parent.parent
    if state_dir / "metrics" / "metrics.db" != db:
        raise ValueError("--db must resolve to the selected state directory")
    return state_dir


def authoritative_scorecard(args: argparse.Namespace) -> dict[str, Any]:
    validate_project_repo(args.project, args.repo)
    state_dir = validate_state_layout(args.db)
    scorecard_project = args.repo if args.project.casefold() == args.repo.rsplit("/", 1)[-1].casefold() else args.project
    result = run([args.shipyard, "--state-dir", str(state_dir), "metrics", "scorecard",
                  "--project", scorecard_project, "--since", f"{args.since_days}d", "--json"], timeout=args.timeout_seconds)
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
    card.pop("database", None)
    return consume_authenticated_strata(card, repo=args.repo, since_days=args.since_days,
                                        reference_at=dt.datetime.now(dt.timezone.utc))


def baseline_readiness(card: dict[str, Any], started: dt.datetime,
                       observed_at: dt.datetime,
                       pass_history: dict[str, Any] | None = None) -> dict[str, Any]:
    """Require the preregistered prospective baseline controls explicitly."""
    reasons: list[dict[str, str]] = []
    retention_secs = (pass_history or {}).get("retention_window_secs", DEFAULT_RETENTION_WINDOW_SECS)
    if not isinstance(retention_secs, int) or isinstance(retention_secs, bool) or retention_secs <= 0:
        retention_secs = DEFAULT_RETENTION_WINDOW_SECS
    effective_started = max(started, observed_at - dt.timedelta(seconds=retention_secs))
    elapsed_days = (observed_at - effective_started).total_seconds() / 86400
    if elapsed_days < 5:
        reasons.append({"status": "insufficient", "reason": "fewer than five continuous elapsed 24-hour days"})

    if not isinstance(pass_history, dict):
        reasons.append({"status": "unavailable", "reason": "prospective pass history is unavailable"})
    else:
        if pass_history.get("status") == "unavailable":
            reasons.append({"status": "unavailable", "reason": "prospective pass history is unavailable"})
        elif not pass_history.get("five_continuous_24h_days"):
            reasons.append({"status": "insufficient", "reason": "prospective pass history has fewer than five continuous elapsed 24-hour days"})
        if pass_history.get("missing_intervals") or pass_history.get("failing_intervals"):
            reasons.append({"status": "unavailable", "reason": "prospective pass history contains missing or failed intervals"})

    instrumentation = card.get("instrumentation_coverage")
    if not isinstance(instrumentation, dict) or instrumentation.get("status") != "available" \
            or instrumentation.get("continuous") is not True \
            or isinstance(instrumentation.get("continuous_days"), bool) \
            or not isinstance(instrumentation.get("continuous_days"), (int, float)) \
            or not math.isfinite(float(instrumentation.get("continuous_days", float("nan")))) \
            or instrumentation.get("continuous_days") < 5:
        reasons.append({"status": "unavailable", "reason": "continuous instrumentation coverage is unavailable"})

    eligibility_reasons: list[dict[str, str]] = []
    for field, minimum, label in (("important_lanes", 30, "important lane"),
                                  ("host_generations", 10, "host generation")):
        coverage = card.get(field)
        if not isinstance(coverage, dict) or not coverage:
            reasons.append({"status": "unavailable", "reason": f"{label} coverage is unavailable"})
            continue
        for name, entry in coverage.items():
            if not isinstance(entry, dict):
                reasons.append({"status": "unavailable", "reason": f"{label} {name} coverage is malformed"})
            elif entry.get("status") == "insufficient":
                eligibility_reasons.append({"status": "insufficient", "reason": f"{label} {name} explicitly insufficient"})
            elif entry.get("status") != "available" or not isinstance(entry.get("completed_jobs"), int) \
                    or isinstance(entry.get("completed_jobs"), bool) or entry["completed_jobs"] < minimum:
                reasons.append({"status": "unavailable", "reason": f"{label} {name} lacks {minimum} completed jobs"})

    strata = card.get("strata")
    sufficiency = card.get("strata_sufficiency")
    if card.get("missing_strata") or not isinstance(sufficiency, dict) \
            or any(name not in sufficiency or not isinstance(sufficiency[name], dict)
                   or sufficiency[name].get("status") != "sufficient" for name in REQUIRED_STRATA):
        reasons.append({"status": "unavailable", "reason": "mandatory strata sample sufficiency is unavailable"})
    for name in REQUIRED_STRATA:
        entry = strata.get(name) if isinstance(strata, dict) else None
        if not isinstance(entry, dict) or entry.get("provenance") != "authenticated" \
                or entry.get("coverage") != "complete":
            reasons.append({"status": "unavailable", "reason": f"mandatory {name} period is unavailable"})

    report_complete = not reasons
    optimization_eligible = report_complete and not eligibility_reasons
    status = "ready" if optimization_eligible else "complete_insufficient" if report_complete else (
        "unavailable" if any(reason["status"] == "unavailable" for reason in reasons)
        else "insufficient")
    return {"status": status, "exit": report_complete, "report_complete": report_complete,
            "optimization_eligible": optimization_eligible, "elapsed_days": elapsed_days,
            "reasons": reasons, "optimization_reasons": eligibility_reasons}


def update_state(path: pathlib.Path, project: str, started: dt.datetime,
                 card: dict[str, Any], events: list[dict[str, Any]],
                 observed_at: dt.datetime | None = None, repo: str | None = None,
                 db: pathlib.Path | None = None,
                 pass_history: dict[str, Any] | None = None,
                 record_observation: bool = True) -> dict[str, Any]:
    state = json.loads(path.read_text()) if path.exists() else {}
    state.setdefault("started_at", started.isoformat())
    state.setdefault("observations", [])
    observed_at = observed_at or dt.datetime.now(dt.timezone.utc)
    if repo is not None and db is not None:
        validate_state_identity(path, project, repo, db)
        state["identity"] = state_identity(path, project, repo, db)
    state["project"] = project
    state["last_observed_at"] = observed_at.isoformat()
    if pass_history is not None:
        state["pass_history"] = pass_history
    imported = any((event.get("new_rows") or 0) > 0 for event in events)
    activity_dates = set()
    for event in events:
        activity_dates.update(value for value in event.get("source_activity_dates", []) if isinstance(value, str))
    baseline_started = dt.datetime.fromisoformat(state["started_at"].replace("Z", "+00:00"))
    baseline_start = baseline_started.date()
    has_activity = imported or bool(activity_dates)
    if has_activity and record_observation:
        state["observations"].append({"at": state["last_observed_at"], "events": events,
                                       "source_activity_dates": sorted(activity_dates), "scorecard": card})
    observed_days = {value for item in state["observations"] for value in item.get("source_activity_dates", [])
                     if value >= baseline_start.isoformat()}
    collection_days = {item["at"][:10] for item in state["observations"]}
    state["collection_days_observed"] = len(collection_days)
    state["source_activity_days_observed"] = len(observed_days)
    # Retain legacy keys as telemetry aliases; readiness no longer uses them.
    state["collection_weekday_days"] = state["collection_days_observed"]
    state["weekday_days_observed"] = state["source_activity_days_observed"]
    state["baseline_readiness"] = baseline_readiness(card, baseline_started, observed_at,
                                                      pass_history if isinstance(pass_history, dict) else None)
    state["baseline_exit"] = state["baseline_readiness"]["exit"]
    payload = json.dumps(state, indent=2, sort_keys=True) + "\n"
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile("w", dir=path.parent, prefix=path.name + ".", delete=False) as temporary:
        temporary.write(payload)
        temporary.flush()
        os.fsync(temporary.fileno())
        temporary_path = temporary.name
    os.replace(temporary_path, path)
    return state


def default_pass_history_path(args: argparse.Namespace) -> pathlib.Path:
    key = f"{args.repo}|{args.source_identity}|{args.config_identity}".encode()
    return args.state.with_name("baseline-pass-history-" + hashlib.sha256(key).hexdigest()[:16] + ".json")


def pass_config_identity(args: argparse.Namespace) -> str:
    return (
        f"{args.config_identity};interval={args.expected_interval_secs};"
        f"slack={args.cadence_slack_secs}"
    )


def record_pass_observation(args: argparse.Namespace, status: str, observed_at: dt.datetime,
                            *, imported_count: int = 0, error: str | None = None) -> dict[str, Any]:
    controller_now = dt.datetime.now(dt.timezone.utc)
    config_identity = pass_config_identity(args)
    if status == "failed":
        observation = PassObservation.failed(
            repo=args.repo, observed_at=observed_at,
            expected_interval_secs=args.expected_interval_secs,
            source_identity=args.source_identity, config_identity=config_identity,
            error=error or "collection failed",
        )
    elif status == "success":
        observation = PassObservation.success(
            repo=args.repo, observed_at=observed_at,
            expected_interval_secs=args.expected_interval_secs,
            source_identity=args.source_identity, config_identity=config_identity,
            imported_count=imported_count,
        )
    else:
        raise ValueError(f"unsupported pass status: {status}")
    history = append_observation(
        args.pass_history_path, observation, repository=args.repo,
        source_identity=args.source_identity, config_identity=config_identity,
        expected_interval_secs=args.expected_interval_secs,
        cadence_slack_secs=args.cadence_slack_secs, now=controller_now,
    )
    baseline_started = (
        dt.datetime.fromisoformat(
            json.loads(args.state.read_text()).get("started_at", observed_at.isoformat()).replace("Z", "+00:00")
        )
        if args.state.exists()
        else observed_at
    )
    if baseline_started >= observed_at:
        baseline_started = observed_at - dt.timedelta(seconds=1)
    summary_started = max(
        baseline_started,
        observed_at - dt.timedelta(seconds=history.retention_window_secs),
    )
    return history.summarize(summary_started, observed_at)


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
    parser.add_argument("--pass-history", type=pathlib.Path,
                        help="prospective pass history sidecar (defaults beside --state)")
    parser.add_argument("--source-identity", default="shipyard.collect_baseline:v1")
    parser.add_argument("--config-identity", default="baseline-policy:v1")
    parser.add_argument("--expected-interval-secs", type=int, default=3600)
    parser.add_argument("--cadence-slack-secs", type=int, default=300)
    args = parser.parse_args()
    if args.timeout_seconds <= 0:
        parser.error("--timeout-seconds must be positive")
    if args.expected_interval_secs <= 0:
        parser.error("--expected-interval-secs must be positive")
    if args.cadence_slack_secs < 0:
        parser.error("--cadence-slack-secs must be non-negative")
    args.pass_history_path = args.pass_history or default_pass_history_path(args)

    events: list[dict[str, Any]] = []
    pass_report: dict[str, Any] | None = None
    pass_recorded = False
    attempt_started_at = dt.datetime.now(dt.timezone.utc)
    try:
        events = import_organic(args) if args.collect else []
        card = authoritative_scorecard(args)
        completed_at = dt.datetime.now(dt.timezone.utc)
        if args.collect:
            counts = [event.get("imported") for event in events if event.get("source") in {"github", "tartci"}]
            imported_count = sum(value for value in counts if isinstance(value, int) and not isinstance(value, bool))
            for event in events:
                event.setdefault("attempt_started_at", attempt_started_at.isoformat())
            state = update_state(args.state, args.project, attempt_started_at, card, events,
                                 observed_at=completed_at, repo=args.repo, db=args.db)
            try:
                pass_report = record_pass_observation(args, "success", completed_at, imported_count=imported_count)
                pass_recorded = True
            except (OSError, HistoryValidationError, ValueError, TypeError) as history_error:
                pass_report = {"status": "unavailable", "reasons": [{"status": "unavailable", "reason": f"prospective pass history unavailable: {history_error}"}]}
            state = update_state(args.state, args.project, attempt_started_at, card, events,
                                 observed_at=completed_at, repo=args.repo, db=args.db,
                                 pass_history=pass_report, record_observation=False)
        else:
            state = update_state(args.state, args.project, completed_at, card, events, observed_at=completed_at,
                                 repo=args.repo, db=args.db, pass_history=pass_report)
    except (OSError, RuntimeError, ValueError, TypeError, KeyError, subprocess.TimeoutExpired) as exc:
        if args.collect and not pass_recorded:
            try:
                record_pass_observation(args, "failed", attempt_started_at, error=str(exc))
            except (OSError, HistoryValidationError, ValueError):
                pass
        print(f"baseline collection failed: {exc}", file=sys.stderr)
        return 1
    output = {"project": args.project, "database": "configured metrics store", "scorecard": card,
              "baseline": {"source_activity_days_observed": state["source_activity_days_observed"],
                           "collection_days_observed": state.get("collection_days_observed", 0),
                           "readiness": state.get("baseline_readiness", {"status": "unavailable"}),
                           "exit": state["baseline_exit"]}, "events": events}
    if args.status_markdown:
        args.status_markdown.write_text(
            "# Organic baseline status\n\n"
            f"- Project: `{args.project}`\n- Observed: `{state['last_observed_at']}`\n"
            f"- Source activity dates observed: **{state['source_activity_days_observed']}** (telemetry only)\n"
            f"- Continuous elapsed days: **{state['baseline_readiness'].get('elapsed_days', 0):.2f}** (exit requires five with active instrumentation)\n"
            f"- Prospective pass history: **{state.get('pass_history', {}).get('continuous_elapsed_days', 0):.2f}** days; trailing interval unobserved: **{state.get('pass_history', {}).get('trailing_interval_unobserved', 'unavailable')}**\n"
            f"- Baseline report complete: **{'yes' if state['baseline_readiness'].get('report_complete') else 'no'}**\n"
            f"- Optimization eligibility: **{'yes' if state['baseline_readiness'].get('optimization_eligible') else 'no'}**\n"
            f"- Safely merged denominator: **{card.get('collector_safe_merge_denominator', {}).get('status', 'unavailable')}**\n"
            f"- Provenance: **{card.get('provenance_status', 'unverified_organic_input')}**.\n"
            f"- Strata: **{len(REQUIRED_STRATA) - len(card.get('missing_strata', REQUIRED_STRATA))}/{len(REQUIRED_STRATA)} sufficient**; authenticated producer records only.\n\n"
            "Synthetic provenance, unavailable phases, and merge denominators are not imputed.\n"
        )
    print(json.dumps(output, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

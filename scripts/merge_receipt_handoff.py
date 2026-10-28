#!/usr/bin/env python3
"""Export validated Shipyard merge receipts and monitor the handoff freshness.

The export is deliberately local and read-only with respect to GitHub: it calls
Shipyard's existing repository-scoped export, then publishes a small health
sidecar. The sidecar gives an observer a durable producer timestamp and an
explicit failure state without manufacturing a merge receipt.
"""
from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import pathlib
import re
import sqlite3
import subprocess
import sys
import tempfile
from typing import Any

SCHEMA = "shipyard-merge-receipt-handoff/v1"
EXIT_STALE = 3
MAX_FUTURE_SKEW_SECS = 300
DEFAULT_BASE_BRANCH = "main"
FRESHNESS_BASIS = "handoff_export_and_source_import"
_REPO = re.compile(r"^[^/\s]+/[^/\s]+$")
HEALTH_FIELDS = frozenset({
    "schema", "repo", "project", "base_branch", "file", "status", "checked_at",
    "exported_at", "latest_merged_at", "source_imported_at", "freshness_basis", "stale_after_seconds", "receipt_count",
    "handoff_bytes", "handoff_sha256", "producer", "error",
})
REQUIRED_HEALTH_FIELDS = frozenset({
    "schema", "repo", "project", "base_branch", "file", "status", "checked_at",
    "exported_at", "latest_merged_at", "source_imported_at", "freshness_basis", "stale_after_seconds",
    "receipt_count", "handoff_bytes", "handoff_sha256", "producer",
})
PRODUCER_FIELDS = frozenset({"identity", "version", "executable_sha256"})


def now() -> dt.datetime:
    return dt.datetime.now(dt.timezone.utc)


def timestamp(value: dt.datetime) -> str:
    return value.astimezone(dt.timezone.utc).isoformat().replace("+00:00", "Z")


def parse_timestamp(value: Any, *, field: str) -> dt.datetime:
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{field} must be an RFC3339 timestamp")
    text = value.strip()
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    try:
        parsed = dt.datetime.fromisoformat(text)
    except ValueError as exc:
        raise ValueError(f"{field} must be an RFC3339 timestamp") from exc
    if parsed.tzinfo is None or parsed.utcoffset() is None:
        raise ValueError(f"{field} must include a timezone")
    return parsed.astimezone(dt.timezone.utc)


def parse_duration(value: str) -> int:
    match = re.fullmatch(r"\s*(\d+(?:\.\d+)?)\s*([smhd])\s*", value)
    if not match:
        raise ValueError("duration must be a positive number followed by s, m, h, or d")
    amount = float(match.group(1))
    if amount <= 0:
        raise ValueError("duration must be positive")
    multiplier = {"s": 1, "m": 60, "h": 3600, "d": 86400}[match.group(2)]
    seconds = int(amount * multiplier)
    if seconds <= 0:
        raise ValueError("duration is too small")
    return seconds


def require_abs_path(path: pathlib.Path, name: str) -> pathlib.Path:
    if not path.is_absolute():
        raise ValueError(f"{name} must be an absolute path")
    return path


def atomic_write(path: pathlib.Path, payload: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile("w", encoding="utf-8", dir=path.parent,
                                     prefix=f".{path.name}.", delete=False) as handle:
        temporary = pathlib.Path(handle.name)
        handle.write(payload)
        handle.flush()
        os.fsync(handle.fileno())
    try:
        os.replace(temporary, path)
    except OSError:
        temporary.unlink(missing_ok=True)
        raise


RECEIPT_FIELDS = frozenset({
    "schema", "receipt_id", "repo", "project", "base_branch", "pr", "head_sha",
    "validated_head_sha", "merge_commit_sha", "merged_at", "safe_merge", "source",
    "required_checks", "gate_receipt_sha256", "evidence",
})


def validate_receipt_record(value: dict[str, Any], *, line_number: int, repo: str,
                            base_branch: str = DEFAULT_BASE_BRANCH,
                            now_value: dt.datetime | None = None) -> None:
    if set(value) != RECEIPT_FIELDS:
        missing = sorted(RECEIPT_FIELDS - set(value))
        extra = sorted(set(value) - RECEIPT_FIELDS)
        raise RuntimeError(f"handoff line {line_number} fields mismatch (missing={missing}, extra={extra})")
    if value["schema"] != "shipyard-merge-receipt/v1":
        raise RuntimeError(f"handoff line {line_number} has an unsupported receipt schema")
    if str(value["repo"]).strip().casefold() != repo.strip().casefold():
        raise RuntimeError(f"handoff line {line_number} names a repository other than {repo}")
    expected_project = repo.rsplit("/", 1)[-1].strip().casefold()
    if not isinstance(value["project"], str) or value["project"].strip().casefold() != expected_project:
        raise RuntimeError(f"handoff line {line_number} project is not bound to repository {repo}")
    if value["base_branch"] != base_branch:
        raise RuntimeError(f"handoff line {line_number} base_branch is not {base_branch}")
    for field in ("receipt_id", "repo", "project", "base_branch", "source", "evidence"):
        if not isinstance(value[field], str) or not value[field].strip():
            raise RuntimeError(f"handoff line {line_number} has an empty {field}")
    if isinstance(value["pr"], bool) or not isinstance(value["pr"], int) or value["pr"] <= 0:
        raise RuntimeError(f"handoff line {line_number} has an invalid pr")
    for field in ("head_sha", "validated_head_sha", "merge_commit_sha"):
        sha = value[field]
        if (not isinstance(sha, str) or len(sha) != 40 or sha.lower() != sha
                or not all(char in "0123456789abcdef" for char in sha)
                or set(sha) == {"0"}):
            raise RuntimeError(f"handoff line {line_number} has an invalid {field}")
    if value["head_sha"] != value["validated_head_sha"]:
        raise RuntimeError(f"handoff line {line_number} has a head mismatch")
    if value["safe_merge"] is not True or value["source"] != "shipyard":
        raise RuntimeError(f"handoff line {line_number} is not a Shipyard safe merge")
    checks = value["required_checks"]
    if (not isinstance(checks, list) or not checks
            or any(not isinstance(check, str) or not check.strip() for check in checks)):
        raise RuntimeError(f"handoff line {line_number} has no required checks")
    digest = value["gate_receipt_sha256"]
    if (not isinstance(digest, str) or len(digest) != 64 or digest.lower() != digest
            or not all(char in "0123456789abcdef" for char in digest)
            or set(digest) == {"0"}):
        raise RuntimeError(f"handoff line {line_number} has an invalid gate receipt hash")
    merged_at = parse_timestamp(value["merged_at"], field="merged_at")
    if now_value is not None and merged_at > now_value + dt.timedelta(seconds=MAX_FUTURE_SKEW_SECS):
        raise RuntimeError(f"handoff line {line_number} merged_at is in the future")


def read_handoff(path: pathlib.Path, repo: str, *, base_branch: str = DEFAULT_BASE_BRANCH,
                 now_value: dt.datetime | None = None) -> tuple[list[dict[str, Any]], bytes]:
    try:
        payload = path.read_bytes()
        text = payload.decode("utf-8")
    except (OSError, UnicodeDecodeError) as exc:
        raise RuntimeError(f"handoff is unreadable: {exc}") from exc
    records: list[dict[str, Any]] = []
    seen_ids: set[str] = set()
    by_pr: dict[tuple[str, int], tuple[str, str, str, str]] = {}
    for line_number, line in enumerate(text.splitlines(), 1):
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError as exc:
            raise RuntimeError(f"handoff line {line_number} is not JSON: {exc.msg}") from exc
        if not isinstance(value, dict):
            raise RuntimeError(f"handoff line {line_number} is not an object")
        validate_receipt_record(value, line_number=line_number, repo=repo, base_branch=base_branch,
                                now_value=now_value)
        receipt_id = value["receipt_id"]
        if receipt_id in seen_ids:
            raise RuntimeError(f"handoff line {line_number} duplicates receipt_id {receipt_id}")
        seen_ids.add(receipt_id)
        identity = (value["project"], value["base_branch"], value["head_sha"], value["merge_commit_sha"])
        pr_key = (value["repo"].strip().casefold(), value["pr"])
        prior = by_pr.get(pr_key)
        if prior is not None and prior != identity:
            raise RuntimeError(f"handoff line {line_number} conflicts with another receipt for PR {value['pr']}")
        by_pr[pr_key] = identity
        records.append(value)
    if not records:
        raise RuntimeError("handoff is empty")
    return records, payload


def read_receipts(path: pathlib.Path, repo: str, *, base_branch: str = DEFAULT_BASE_BRANCH,
                  now_value: dt.datetime | None = None) -> list[dict[str, Any]]:
    return read_handoff(path, repo, base_branch=base_branch, now_value=now_value)[0]


def payload_digest(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def producer_identity(shipyard: pathlib.Path) -> dict[str, str]:
    try:
        digest = payload_digest(shipyard.read_bytes())
        completed = subprocess.run([str(shipyard), "--version"], text=True,
                                   capture_output=True, timeout=10, check=False)
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise RuntimeError(f"could not identify Shipyard producer: {exc}") from exc
    version = (completed.stdout or "").strip().splitlines()
    if completed.returncode != 0 or not version:
        raise RuntimeError("could not identify Shipyard producer version")
    return {"identity": "shipyard", "version": version[0], "executable_sha256": digest}


def source_imported_at(state_dir: pathlib.Path, repo: str, base_branch: str) -> str:
    """Read the producer DB's newest receipt import time without mutating it."""
    database = state_dir / "metrics" / "metrics.db"
    try:
        with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
            columns = {row[1] for row in connection.execute("pragma table_info(merge_receipts)")}
            if "imported_at" not in columns:
                raise RuntimeError("producer merge receipt store has no imported_at column")
            value = connection.execute(
                "SELECT max(imported_at) FROM merge_receipts "
                "WHERE lower(trim(repo)) = lower(trim(?1)) AND project = ?2 AND base_branch = ?3",
                (repo, repo.rsplit("/", 1)[-1], base_branch),
            ).fetchone()[0]
    except (OSError, sqlite3.Error) as exc:
        raise RuntimeError(f"producer merge receipt source is unreadable: {exc}") from exc
    if not isinstance(value, str) or not value.strip():
        raise RuntimeError("producer merge receipt source has no imported_at for repository/base branch")
    try:
        return timestamp(parse_timestamp(value, field="source_imported_at"))
    except ValueError as exc:
        raise RuntimeError(str(exc)) from exc


def health_payload(*, repo: str, file: pathlib.Path, checked_at: dt.datetime,
                   base_branch: str = DEFAULT_BASE_BRANCH,
                   status: str, exported_at: dt.datetime | None = None,
                   source_imported_at: str | None = None,
                   stale_after_seconds: int | None = None,
                   receipts: list[dict[str, Any]] | None = None,
                   payload: bytes | None = None, producer: dict[str, str] | None = None,
                   error: str | None = None) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "schema": SCHEMA,
        "repo": repo,
        "project": repo.rsplit("/", 1)[-1],
        "base_branch": base_branch,
        "file": str(file),
        "status": status,
        "freshness_basis": FRESHNESS_BASIS,
        "checked_at": timestamp(checked_at),
        "receipt_count": len(receipts or []),
        "handoff_bytes": len(payload) if payload is not None else None,
        "handoff_sha256": payload_digest(payload) if payload is not None else None,
        "producer": producer or {"identity": "shipyard", "version": "unavailable", "executable_sha256": "unavailable"},
    }
    if exported_at is not None:
        payload["exported_at"] = timestamp(exported_at)
    if source_imported_at is not None:
        payload["source_imported_at"] = source_imported_at
    if stale_after_seconds is not None:
        payload["stale_after_seconds"] = stale_after_seconds
    if receipts:
        payload["latest_merged_at"] = max(
            (parse_timestamp(record["merged_at"], field="merged_at") for record in receipts),
            default=checked_at,
        ).isoformat().replace("+00:00", "Z")
    if error:
        payload["error"] = error
    return payload


def write_health(path: pathlib.Path, payload: dict[str, Any]) -> None:
    atomic_write(path, json.dumps(payload, indent=2, sort_keys=True) + "\n")


def export(args: argparse.Namespace) -> int:
    shipyard = require_abs_path(args.shipyard, "--shipyard")
    state_dir = require_abs_path(args.state_dir, "--state-dir")
    file = require_abs_path(args.file, "--file")
    health_file = require_abs_path(args.health_file, "--health-file")
    stale_after_seconds = parse_duration(args.stale_after)
    if not shipyard.is_file() or not os.access(shipyard, os.X_OK):
        raise ValueError(f"Shipyard executable is unavailable: {shipyard}")
    if not _REPO.fullmatch(args.repo):
        raise ValueError("--repo must be OWNER/REPO")
    file.parent.mkdir(parents=True, exist_ok=True)
    staged = tempfile.NamedTemporaryFile("wb", dir=file.parent, prefix=f".{file.name}.", delete=False)
    staged_path = pathlib.Path(staged.name)
    staged.close()
    command = [str(shipyard), "--state-dir", str(state_dir), "metrics", "export",
               "merge-receipt", "--repo", args.repo, "--file", str(staged_path)]
    completed: subprocess.CompletedProcess[str] | None = None
    try:
        source_before = source_imported_at(state_dir, args.repo, args.base_branch)
        completed = subprocess.run(command, text=True, capture_output=True,
                                   timeout=args.timeout_seconds, check=False)
        if completed.returncode != 0:
            detail = (completed.stderr or completed.stdout).strip().splitlines()
            error = detail[-1] if detail else f"Shipyard export exited {completed.returncode}"
            raise RuntimeError(error)
        exported = now()
        receipts, payload = read_handoff(staged_path, args.repo, base_branch=args.base_branch,
                                         now_value=exported)
        imported_at = source_imported_at(state_dir, args.repo, args.base_branch)
        if imported_at != source_before:
            raise RuntimeError("producer merge receipt source changed during export")
        producer = producer_identity(shipyard)
        health = health_payload(repo=args.repo, file=file, base_branch=args.base_branch,
                                checked_at=exported, status="fresh",
                                exported_at=exported, source_imported_at=imported_at,
                                stale_after_seconds=stale_after_seconds,
                                receipts=receipts, payload=payload,
                                producer=producer)
        # Validation completed against the staged payload; publish only after
        # both the exact receipt set and producer identity are known-good.
        os.replace(staged_path, file)
        write_health(health_file, health)
        print(json.dumps(health, sort_keys=True))
        return 0
    except subprocess.TimeoutExpired as exc:
        error = f"export timed out after {args.timeout_seconds}s"
        write_health(health_file, health_payload(repo=args.repo, file=file,
                                                  checked_at=now(), status="error", error=error))
        raise RuntimeError(error) from exc
    except (RuntimeError, ValueError) as exc:
        # A non-zero producer command is an observed producer error and gets
        # a durable error sidecar. A zero-exit command whose staged payload or
        # producer identity is malformed leaves the prior payload and health
        # untouched, so observers cannot mistake a rejected refresh for a new
        # handoff.
        if completed is None or completed.returncode != 0:
            write_health(health_file, health_payload(repo=args.repo, file=file,
                                                      checked_at=now(), status="error", error=str(exc)))
        raise
    finally:
        staged_path.unlink(missing_ok=True)


def check(args: argparse.Namespace) -> int:
    file = require_abs_path(args.file, "--file")
    health_file = require_abs_path(args.health_file, "--health-file")
    threshold = parse_duration(args.stale_after)
    checked = now()
    try:
        if not _REPO.fullmatch(args.repo):
            raise RuntimeError("requested repository must be OWNER/REPO")
        health = json.loads(health_file.read_text(encoding="utf-8"))
        if not isinstance(health, dict) or health.get("schema") != SCHEMA:
            raise RuntimeError("health sidecar has an unsupported schema")
        if health.get("status") != "fresh":
            raise RuntimeError(f"health sidecar status is {health.get('status', 'unknown')}")
        if not REQUIRED_HEALTH_FIELDS.issubset(health):
            raise RuntimeError("health sidecar is missing required fields")
        if health.get("stale_after_seconds") != threshold:
            raise RuntimeError("health sidecar freshness window does not match requested policy")
        repo = health.get("repo")
        if not isinstance(repo, str) or not _REPO.fullmatch(repo):
            raise RuntimeError("health sidecar has no valid repo")
        if repo.casefold() != args.repo.casefold():
            raise RuntimeError("health sidecar repository does not match requested repository")
        if health.get("project") != repo.rsplit("/", 1)[-1]:
            raise RuntimeError("health sidecar project is not bound to repository")
        recorded_file = health.get("file")
        if not isinstance(recorded_file, str) or pathlib.Path(recorded_file).resolve() != file.resolve():
            raise RuntimeError("health sidecar is bound to a different handoff file")
        if set(health) - HEALTH_FIELDS:
            raise RuntimeError("health sidecar contains unknown fields")
        if health.get("freshness_basis") != FRESHNESS_BASIS:
            raise RuntimeError("health sidecar has an unsupported freshness basis")
        expected_base = health.get("base_branch")
        if not isinstance(expected_base, str) or expected_base != args.base_branch:
            raise RuntimeError("health sidecar base branch does not match requested base")
        records, handoff_payload = read_handoff(file, repo, base_branch=expected_base, now_value=checked)
        checked_at = parse_timestamp(health.get("checked_at"), field="checked_at")
        if checked_at > checked + dt.timedelta(seconds=MAX_FUTURE_SKEW_SECS):
            raise RuntimeError("health sidecar checked_at is in the future")
        exported_at = parse_timestamp(health.get("exported_at"), field="exported_at")
        source_at = parse_timestamp(health.get("source_imported_at"), field="source_imported_at")
        if exported_at > checked + dt.timedelta(seconds=MAX_FUTURE_SKEW_SECS):
            raise RuntimeError("health sidecar exported_at is in the future")
        if source_at > checked + dt.timedelta(seconds=MAX_FUTURE_SKEW_SECS):
            raise RuntimeError("health sidecar source_imported_at is in the future")
        latest = parse_timestamp(health.get("latest_merged_at"), field="latest_merged_at")
        expected_latest = max(parse_timestamp(record["merged_at"], field="merged_at") for record in records)
        if latest != expected_latest:
            raise RuntimeError("health sidecar latest_merged_at does not match handoff")
        expected_bytes = health.get("handoff_bytes")
        expected_sha = health.get("handoff_sha256")
        if isinstance(expected_bytes, bool) or not isinstance(expected_bytes, int) or expected_bytes != len(handoff_payload):
            raise RuntimeError("health sidecar byte count does not match handoff")
        if not isinstance(expected_sha, str) or expected_sha != payload_digest(handoff_payload):
            raise RuntimeError("health sidecar digest does not match handoff")
        if health.get("receipt_count") != len(records):
            raise RuntimeError("health sidecar record count does not match handoff")
        producer = health.get("producer")
        if isinstance(producer, dict) and set(producer) != PRODUCER_FIELDS:
            raise RuntimeError("health sidecar producer contains unknown fields")
        producer_digest = producer.get("executable_sha256") if isinstance(producer, dict) else None
        producer_version = producer.get("version") if isinstance(producer, dict) else None
        if (not isinstance(producer, dict) or producer.get("identity") != "shipyard"
                or not isinstance(producer_version, str) or not producer_version.strip()
                or producer_version == "unavailable"
                or not isinstance(producer_digest, str) or len(producer_digest) != 64
                or producer_digest.lower() != producer_digest
                or not all(char in "0123456789abcdef" for char in producer_digest)
                or set(producer_digest) == {"0"}):
            raise RuntimeError("health sidecar has an unavailable or invalid producer identity")
        freshness_at = min(exported_at, source_at)
        status = "fresh" if (checked - freshness_at).total_seconds() <= threshold else "stale"
        payload = dict(health)
        payload.update({"status": status, "checked_at": timestamp(checked),
                        "age_seconds": max(0, int((checked - freshness_at).total_seconds())),
                        "stale_after_seconds": threshold,
                        "receipt_count": len(records)})
    except (OSError, json.JSONDecodeError, RuntimeError, ValueError) as exc:
        payload = {"schema": SCHEMA, "status": "error", "checked_at": timestamp(checked),
                   "file": str(file), "health_file": str(health_file), "error": str(exc),
                   "stale_after_seconds": threshold}
        print(json.dumps(payload, sort_keys=True))
        return EXIT_STALE
    print(json.dumps(payload, sort_keys=True))
    return 0 if payload["status"] == "fresh" else EXIT_STALE


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    commands = root.add_subparsers(dest="command", required=True)
    export_parser = commands.add_parser("export", help="run the local Shipyard export and publish health")
    export_parser.add_argument("--shipyard", type=pathlib.Path, required=True)
    export_parser.add_argument("--state-dir", type=pathlib.Path, required=True)
    export_parser.add_argument("--repo", required=True)
    export_parser.add_argument("--base-branch", default=DEFAULT_BASE_BRANCH)
    export_parser.add_argument("--stale-after", default="2h")
    export_parser.add_argument("--file", type=pathlib.Path, required=True)
    export_parser.add_argument("--health-file", type=pathlib.Path, required=True)
    export_parser.add_argument("--timeout-seconds", type=int, default=300)
    export_parser.set_defaults(handler=export)
    check_parser = commands.add_parser("check", help="check a handoff and its producer health sidecar")
    check_parser.add_argument("--file", type=pathlib.Path, required=True)
    check_parser.add_argument("--repo", required=True)
    check_parser.add_argument("--base-branch", default=DEFAULT_BASE_BRANCH)
    check_parser.add_argument("--health-file", type=pathlib.Path, required=True)
    check_parser.add_argument("--stale-after", default="2h")
    check_parser.set_defaults(handler=check)
    return root


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    if getattr(args, "timeout_seconds", 1) <= 0:
        parser().error("--timeout-seconds must be positive")
    try:
        return args.handler(args)
    except (OSError, RuntimeError, ValueError) as exc:
        print(f"merge receipt handoff failed: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Export validated Shipyard merge receipts and monitor the handoff freshness.

The export is deliberately local and read-only with respect to GitHub: it calls
Shipyard's existing repository-scoped export, then publishes a small health
sidecar.  The sidecar lets an observer distinguish a healthy empty period from a
producer that stopped running, without manufacturing a merge receipt.
"""
from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile
from typing import Any

SCHEMA = "shipyard-merge-receipt-handoff/v1"
EXIT_STALE = 3
_REPO = re.compile(r"^[^/\s]+/[^/\s]+$")


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


def read_receipts(path: pathlib.Path, repo: str) -> list[dict[str, Any]]:
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise RuntimeError(f"handoff is unreadable: {exc}") from exc
    records: list[dict[str, Any]] = []
    for line_number, line in enumerate(text.splitlines(), 1):
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError as exc:
            raise RuntimeError(f"handoff line {line_number} is not JSON: {exc.msg}") from exc
        if not isinstance(value, dict):
            raise RuntimeError(f"handoff line {line_number} is not an object")
        if str(value.get("repo", "")).strip().casefold() != repo.strip().casefold():
            raise RuntimeError(f"handoff line {line_number} names a repository other than {repo}")
        parse_timestamp(value.get("merged_at"), field="merged_at")
        if not isinstance(value.get("receipt_id"), str) or not value["receipt_id"].strip():
            raise RuntimeError(f"handoff line {line_number} has no receipt_id")
        records.append(value)
    if not records:
        raise RuntimeError("handoff is empty")
    return records


def health_payload(*, repo: str, file: pathlib.Path, checked_at: dt.datetime,
                   status: str, exported_at: dt.datetime | None = None,
                   receipts: list[dict[str, Any]] | None = None,
                   error: str | None = None) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "schema": SCHEMA,
        "repo": repo,
        "file": str(file),
        "status": status,
        "checked_at": timestamp(checked_at),
        "receipt_count": len(receipts or []),
    }
    if exported_at is not None:
        payload["exported_at"] = timestamp(exported_at)
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
    if not shipyard.is_file() or not os.access(shipyard, os.X_OK):
        raise ValueError(f"Shipyard executable is unavailable: {shipyard}")
    if not _REPO.fullmatch(args.repo):
        raise ValueError("--repo must be OWNER/REPO")
    started = now()
    command = [str(shipyard), "--state-dir", str(state_dir), "metrics", "export",
               "merge-receipt", "--repo", args.repo, "--file", str(file)]
    try:
        completed = subprocess.run(command, text=True, capture_output=True,
                                   timeout=args.timeout_seconds, check=False)
    except subprocess.TimeoutExpired as exc:
        error = f"export timed out after {args.timeout_seconds}s"
        write_health(health_file, health_payload(repo=args.repo, file=file,
                                                  checked_at=now(), status="error", error=error))
        raise RuntimeError(error) from exc
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).strip().splitlines()
        error = detail[-1] if detail else f"Shipyard export exited {completed.returncode}"
        write_health(health_file, health_payload(repo=args.repo, file=file,
                                                  checked_at=now(), status="error", error=error))
        raise RuntimeError(error)
    receipts = read_receipts(file, args.repo)
    exported = now()
    write_health(health_file, health_payload(repo=args.repo, file=file,
                                              checked_at=exported, status="fresh",
                                              exported_at=exported, receipts=receipts))
    print(json.dumps(health_payload(repo=args.repo, file=file, checked_at=exported,
                                    status="fresh", exported_at=exported,
                                    receipts=receipts), sort_keys=True))
    return 0


def check(args: argparse.Namespace) -> int:
    file = require_abs_path(args.file, "--file")
    health_file = require_abs_path(args.health_file, "--health-file")
    threshold = parse_duration(args.stale_after)
    checked = now()
    try:
        health = json.loads(health_file.read_text(encoding="utf-8"))
        if not isinstance(health, dict) or health.get("schema") != SCHEMA:
            raise RuntimeError("health sidecar has an unsupported schema")
        repo = health.get("repo")
        if not isinstance(repo, str) or not _REPO.fullmatch(repo):
            raise RuntimeError("health sidecar has no valid repo")
        recorded_file = health.get("file")
        if not isinstance(recorded_file, str) or pathlib.Path(recorded_file).resolve() != file.resolve():
            raise RuntimeError("health sidecar is bound to a different handoff file")
        records = read_receipts(file, repo)
        exported_at = parse_timestamp(health.get("exported_at"), field="exported_at")
        age = max(0.0, (checked - exported_at).total_seconds())
        status = "fresh" if age <= threshold else "stale"
        payload = dict(health)
        payload.update({"status": status, "checked_at": timestamp(checked),
                        "age_seconds": int(age), "stale_after_seconds": threshold,
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
    export_parser.add_argument("--file", type=pathlib.Path, required=True)
    export_parser.add_argument("--health-file", type=pathlib.Path, required=True)
    export_parser.add_argument("--timeout-seconds", type=int, default=300)
    export_parser.set_defaults(handler=export)
    check_parser = commands.add_parser("check", help="check a handoff and its producer health sidecar")
    check_parser.add_argument("--file", type=pathlib.Path, required=True)
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

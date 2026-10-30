#!/usr/bin/env python3
"""One bounded capacity observation pass; no scheduling or admission authority."""
from __future__ import annotations

import argparse
import asyncio
import datetime as dt
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shlex
import signal
import sys
import tempfile
import time
import uuid

SCHEMA = "tartci.fleet-capacity/v1"
ROSTER_SCHEMA = "shipyard.capacity-roster/v1"
STATUS_SCHEMA = "shipyard.capacity-observation/v1"
MAX_HOSTS = 16
ROSTER_BYTES = 65536
# Completeness only. Rust remains the sole digest, schema and capacity validator.
ENVELOPE = frozenset("schema sample_id observed_at valid_until host_id host_boot_id generation repo target "
                     "profile_sha256 configured_slots eligible_slots free_slots occupied_slots in_flight_slots "
                     "unknown_slots pool_state source auth_method verification evidence evidence_sha256 census_sha256".split())
TOKEN = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}\Z")
REPO = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9][A-Za-z0-9_.-]*\Z")
TARGET = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.:/-]{0,127}\Z")


class ObservationError(ValueError):
    pass


def now():
    return dt.datetime.now(dt.timezone.utc)


def stamp(value):
    return value.astimezone(dt.timezone.utc).isoformat()


def timestamp(value):
    if not isinstance(value, str):
        raise ObservationError("invalid_timestamp")
    try:
        parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
        if parsed.utcoffset() is None:
            raise ValueError("timezone required")
        return parsed.astimezone(dt.timezone.utc)
    except (ValueError, OverflowError) as exc:
        raise ObservationError("invalid_timestamp") from exc


def encode(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":"), allow_nan=False) + "\n").encode()


def strict_json(payload):
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ObservationError("duplicate_json_key")
            result[key] = value
        return result

    def constant(_):
        raise ObservationError("nonfinite_json")

    try:
        return json.loads(payload, object_pairs_hook=pairs, parse_constant=constant)
    except (UnicodeError, ValueError, RecursionError) as exc:
        raise ObservationError("invalid_json") from exc


def bounded_file(path, limit):
    with path.open("rb") as stream:
        payload = stream.read(limit + 1)
    if len(payload) > limit:
        raise ObservationError("file_byte_limit")
    return payload


def read_roster(path):
    raw = bounded_file(path, ROSTER_BYTES)
    value = strict_json(raw)
    if not isinstance(value, dict) or set(value) != {"schema", "hosts"} or value["schema"] != ROSTER_SCHEMA:
        raise ObservationError("invalid_roster")
    hosts = value["hosts"]
    if not isinstance(hosts, list) or not 1 <= len(hosts) <= MAX_HOSTS:
        raise ObservationError("invalid_roster_size")
    seen_aliases, seen_hosts = set(), set()
    for host in hosts:
        if not isinstance(host, dict) or set(host) != {"ssh_alias", "host_id", "tartci", "repo", "target"}:
            raise ObservationError("invalid_roster_host")
        if not all(isinstance(v, str) and 0 < len(v) <= 512 for v in host.values()):
            raise ObservationError("invalid_roster_value")
        if not TOKEN.fullmatch(host["ssh_alias"]) or not TOKEN.fullmatch(host["host_id"]):
            raise ObservationError("invalid_host_identity")
        if not REPO.fullmatch(host["repo"]) or not TARGET.fullmatch(host["target"]):
            raise ObservationError("invalid_repo_target")
        if (not host["tartci"].startswith("/") or ".." in Path(host["tartci"]).parts
                or any(ord(c) < 32 for c in host["tartci"])):
            raise ObservationError("invalid_tartci_path")
        if host["ssh_alias"] in seen_aliases or host["host_id"] in seen_hosts:
            raise ObservationError("duplicate_roster_host")
        seen_aliases.add(host["ssh_alias"])
        seen_hosts.add(host["host_id"])
    return hosts, hashlib.sha256(raw).hexdigest()


def ssh_command(ssh, host):
    remote = shlex.join([host["tartci"], "fleet", "capacity", "--repo", host["repo"],
                         "--target", host["target"], "--valid-for-seconds", "120", "--json"])
    return [ssh, "-n", "-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5",
            "-o", "ConnectionAttempts=1", "-o", "ServerAliveInterval=5",
            "-o", "ServerAliveCountMax=1", host["ssh_alias"], remote]


async def bounded_read(stream, limit):
    chunks, size = [], 0
    while True:
        chunk = await stream.read(min(16384, limit + 1 - size))
        if not chunk:
            return b"".join(chunks)
        size += len(chunk)
        if size > limit:
            raise ObservationError("output_byte_limit")
        chunks.append(chunk)


async def command(argv, deadline, output_bytes, stdin=None):
    if time.monotonic() >= deadline:
        raise ObservationError("deadline")
    try:
        process = await asyncio.create_subprocess_exec(
            *argv, stdin=asyncio.subprocess.PIPE if stdin is not None else asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE, start_new_session=True)
    except OSError as exc:
        raise ObservationError("spawn_failed") from exc

    async def send():
        if stdin is not None:
            try:
                process.stdin.write(stdin)
                await process.stdin.drain()
            except (BrokenPipeError, ConnectionResetError):
                pass
            finally:
                process.stdin.close()

    tasks = [asyncio.create_task(bounded_read(process.stdout, output_bytes)),
             asyncio.create_task(bounded_read(process.stderr, 16384)),
             asyncio.create_task(send()), asyncio.create_task(process.wait())]
    try:
        result = await asyncio.wait_for(asyncio.gather(*tasks), max(0.001, deadline - time.monotonic()))
        return result[3], result[0]
    except asyncio.TimeoutError as exc:
        raise ObservationError("deadline") from exc
    finally:
        # This session/group was created by this call. Never signal inherited groups.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        try:
            await asyncio.wait_for(process.wait(), 0.5)
        except asyncio.TimeoutError as exc:
            raise ObservationError("cleanup_timeout") from exc


def receipt_envelope(payload, code, host, checked, skew):
    if code not in (0, 2):
        raise ObservationError("transport_or_producer_exit")
    receipt = strict_json(payload)
    if not isinstance(receipt, dict) or not ENVELOPE.issubset(receipt):
        raise ObservationError("incomplete_envelope")
    if (receipt["schema"] != SCHEMA or receipt["source"] != "tartci"
            or receipt["auth_method"] != "untrusted_local"):
        raise ObservationError("unsupported_envelope")
    for key in ("host_id", "repo", "target"):
        if receipt[key] != host[key]:
            raise ObservationError("identity_mismatch")
    if (code, receipt["verification"]) not in ((0, "verified"), (2, "unknown")):
        raise ObservationError("exit_verification_mismatch")
    for key in ("sample_id", "host_boot_id", "generation"):
        if not isinstance(receipt[key], str) or not 0 < len(receipt[key]) <= 256:
            raise ObservationError("invalid_identity_envelope")
    if not isinstance(receipt["evidence"], dict):
        raise ObservationError("incomplete_envelope")
    observed, valid = timestamp(receipt["observed_at"]), timestamp(receipt["valid_until"])
    if observed > checked + dt.timedelta(seconds=skew):
        raise ObservationError("future_observation")
    if valid <= checked or observed < checked - dt.timedelta(seconds=120):
        raise ObservationError("stale_observation")
    if not 0 < (valid - observed).total_seconds() <= 120:
        raise ObservationError("unexpected_validity")
    return receipt


async def collect(args, hosts, pass_deadline):
    fetch_deadline = min(pass_deadline - 2, time.monotonic() + args.fetch_seconds)
    semaphore = asyncio.Semaphore(args.parallel)

    async def fetch(host):
        row = {"host_id": host["host_id"], "repo": host["repo"], "target": host["target"],
               "status": "missing", "import": "not_submitted"}
        async with semaphore:
            try:
                code, payload = await command(ssh_command(args.ssh, host), fetch_deadline, args.receipt_bytes)
                value = receipt_envelope(payload, code, host, now(), args.clock_skew_seconds)
                row.update(status="received", verification=value["verification"],
                           sample_id=value["sample_id"], generation=value["generation"],
                           host_boot_id=value["host_boot_id"], bytes=len(payload))
                return row, payload, code
            except ObservationError as exc:
                row["reason"] = str(exc)
                return row, None, None

    fetched = await asyncio.gather(*(fetch(host) for host in hosts))
    batch, submitted = [], []
    for host, (row, payload, code) in zip(hosts, fetched):
        if payload is None:
            continue
        try:
            value = receipt_envelope(payload, code, host, now(), args.clock_skew_seconds)
            if timestamp(value["valid_until"]) <= now() + dt.timedelta(seconds=max(0, pass_deadline - time.monotonic())):
                raise ObservationError("expires_before_import_deadline")
        except ObservationError as exc:
            row.update(status="missing", reason=str(exc))
            continue
        batch.append(payload.strip())
        submitted.append(row)
    import_status = "not_attempted"
    if batch:
        # Preserve producer JSON bytes; do not recompute digests or rewrite receipts.
        payload = b"[" + b",".join(batch) + b"]\n"
        for row in submitted:
            row["import"] = "unconfirmed"
        try:
            code, output = await command([args.shipyard, "--state-dir", str(args.state_dir),
                                         "metrics", "import", "fleet-capacity", "--json"],
                                        pass_deadline - 1, 65536, payload)
            result = strict_json(output)
            if (code != 0 or not isinstance(result, dict) or result.get("source") != "fleet-capacity"
                    or type(result.get("imported")) is not int or not 0 <= result["imported"] <= len(batch)):
                raise ObservationError("import_not_confirmed")
            import_status = "accepted"
            for row in submitted:
                row["import"] = "accepted"
        except ObservationError as exc:
            # A killed importer might have committed. Never claim rollback or retry automatically.
            import_status = str(exc)
    return [row for row, _, _ in fetched], import_status


def atomic_write(path, payload):
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=path.parent, prefix="." + path.name, delete=False) as stream:
            temporary = Path(stream.name)
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def persist(args, record):
    # A separate short lock lets overlapping attempts record their skip without
    # entering the collection lock or holding the database writer lease.
    with (args.output_dir / "journal.lock").open("a+b") as lock:
        until = time.monotonic() + 1
        while True:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= until:
                    raise ObservationError("journal_busy")
                time.sleep(0.01)
        path = args.output_dir / "attempts.jsonl"
        previous = bounded_file(path, 16777216) if path.exists() else b""
        cutoff = now() - dt.timedelta(days=args.history_days)
        rows = []
        for line in previous.splitlines():
            value = strict_json(line)
            if (not isinstance(value, dict) or value.get("schema") != STATUS_SCHEMA
                    or not isinstance(value.get("hosts"), list)
                    or any(not isinstance(host, dict) for host in value["hosts"])):
                raise ObservationError("invalid_journal")
            if timestamp(value.get("completed_at")) >= cutoff:
                rows.append(value)
        # Equality only: generation strings/hashes carry no ordering semantics.
        for host in record["hosts"]:
            if host.get("import") != "accepted":
                continue
            for old in reversed(rows):
                if old.get("roster_sha256") != record["roster_sha256"]:
                    continue
                match = next((item for item in old.get("hosts", []) if item.get("host_id") == host["host_id"]
                              and item.get("import") == "accepted"), None)
                if match:
                    host["identity_changed"] = any(host[key] != match.get(key) for key in ("generation", "host_boot_id"))
                    break
        encoded = [encode(row) for row in rows] + [encode(record)]
        size = sum(map(len, encoded))
        while len(encoded) > 1 and size > args.history_bytes:
            size -= len(encoded.pop(0))
        if size > args.history_bytes:
            raise ObservationError("journal_record_too_large")
        atomic_write(path, b"".join(encoded))
        atomic_write(args.output_dir / "last-status.json", encode(record))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--roster", type=Path, required=True)
    parser.add_argument("--state-dir", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--ssh", default="ssh")
    parser.add_argument("--shipyard", default="shipyard")
    parser.add_argument("--deadline-seconds", type=float, default=55)
    parser.add_argument("--fetch-seconds", type=float, default=20)
    parser.add_argument("--parallel", type=int, default=4)
    parser.add_argument("--receipt-bytes", type=int, default=262144)
    parser.add_argument("--clock-skew-seconds", type=int, default=5)
    parser.add_argument("--expected-interval-seconds", type=int, default=60)
    parser.add_argument("--history-days", type=int, default=7)
    parser.add_argument("--history-bytes", type=int, default=1048576)
    args = parser.parse_args(argv)
    if (not math.isfinite(args.deadline_seconds) or not 3 <= args.deadline_seconds <= 55
            or not math.isfinite(args.fetch_seconds) or not 0 < args.fetch_seconds < args.deadline_seconds - 2
            or not 1 <= args.parallel <= MAX_HOSTS or not 1024 <= args.receipt_bytes <= 1048576
            or not 0 <= args.clock_skew_seconds <= 60 or args.expected_interval_seconds < 60
            or not 1 <= args.history_days <= 36500 or not 65536 <= args.history_bytes <= 16777216):
        parser.error("invalid bounded collection policy")
    started, monotonic_start = now(), time.monotonic()
    try:
        hosts, roster_sha = read_roster(args.roster)
        args.output_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        record = {"schema": STATUS_SCHEMA, "pass_id": uuid.uuid4().hex,
                  "started_at": stamp(started), "roster_sha256": roster_sha,
                  "source_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                  "expected_interval_seconds": args.expected_interval_seconds,
                  "policy": {key: getattr(args, key) for key in ("deadline_seconds", "fetch_seconds", "parallel",
                              "receipt_bytes", "clock_skew_seconds", "history_days", "history_bytes")},
                  "status": "partial", "hosts": []}
        with (args.output_dir / "pass.lock").open("a+b") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                record.update(status="skipped_overlap", import_status="not_attempted",
                              hosts=[{"host_id": h["host_id"], "repo": h["repo"], "target": h["target"],
                                      "status": "missing", "reason": "skipped_overlap", "import": "not_submitted"}
                                     for h in hosts])
            else:
                record["hosts"], record["import_status"] = asyncio.run(
                    collect(args, hosts, monotonic_start + args.deadline_seconds))
                if all(h["import"] == "accepted" and h.get("verification") == "verified" for h in record["hosts"]):
                    record["status"] = "complete"
            record["completed_at"] = stamp(now())
            record["elapsed_seconds"] = round(time.monotonic() - monotonic_start, 3)
            persist(args, record)
        print(encode(record).decode(), end="")
        return 0 if record["status"] == "complete" else 2
    except (OSError, ObservationError) as exc:
        print(f"capacity observation failed: {type(exc).__name__}: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

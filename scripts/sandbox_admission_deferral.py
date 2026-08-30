#!/usr/bin/env python3
"""Validate a guardian receipt that safely deferred a production-host canary."""

from __future__ import annotations

import argparse
import json
import os
import re
import stat
import sys
from pathlib import Path
from typing import Any


ACTIVE_WORKER_FAILURE = "GuardianError: refusing canary transition with active workers:"
RETAINED_RECONCILIATION_REASON = "retained-lease-awaiting-idle"
SHA256 = re.compile(r"[0-9a-f]{64}\Z")
LEASE_GENERATION_MARKER = ".shipyard-lease-generation.json"


class DeferralError(ValueError):
    """The receipt does not prove a safe admission deferral."""


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise DeferralError(message)


def validate_deferral(
    receipt: Any,
    *,
    installed_sha256: str,
    canary_root: Path,
    lease_dir: Path | None = None,
) -> dict[str, Any]:
    _require(isinstance(receipt, dict), "receipt must be a JSON object")
    if receipt.get("reason") == RETAINED_RECONCILIATION_REASON:
        _require(lease_dir is not None, "retained reconciliation requires lease dir")
        return validate_retained_reconciliation(
            receipt,
            installed_sha256=installed_sha256,
            canary_root=canary_root,
            lease_dir=lease_dir,
        )
    required_fields = {
        "schema_version",
        "reason",
        "failure",
        "candidate_stopped",
        "lease_removed",
        "lease_retained",
        "mode",
        "active_runs",
        "old_production_pid",
        "old_production_start_time",
        "installed_sha256",
        "transition_path",
        "production_quiesced",
        "production_restored",
        "production_preserved",
        "mutation_fence_proved",
        "old_lifetime_lock_owned",
        "mutation_probe_output",
    }
    missing_fields = sorted(required_fields.difference(receipt))
    _require(not missing_fields, f"missing required fields: {missing_fields}")
    _require(SHA256.fullmatch(installed_sha256) is not None, "invalid expected hash")
    _require(canary_root.is_absolute(), "canary root must be absolute")
    expected_root = Path(os.path.normpath(canary_root))
    _require(expected_root == canary_root, "canary root must be normalized")

    active_runs = receipt.get("active_runs")
    _require(
        isinstance(active_runs, list)
        and bool(active_runs)
        and all(isinstance(run, str) and bool(run) for run in active_runs),
        "active_runs must contain non-empty strings",
    )
    pid = receipt.get("old_production_pid")
    _require(isinstance(pid, int) and not isinstance(pid, bool) and pid > 1, "invalid pid")
    start_time = receipt.get("old_production_start_time")
    _require(isinstance(start_time, str) and bool(start_time), "invalid start time")
    failure = receipt.get("failure")
    _require(
        isinstance(failure, str) and failure.startswith(ACTIVE_WORKER_FAILURE),
        "failure is not the active-worker admission refusal",
    )
    _require(receipt.get("schema_version") == 1, "unsupported receipt schema")
    _require(receipt.get("reason") == "failed", "unexpected receipt reason")
    _require(receipt.get("candidate_stopped") is True, "candidate was not stopped")
    _require(receipt.get("lease_removed") is True, "lease was not removed")
    _require(receipt.get("lease_retained") is False, "lease remains retained")
    _require(receipt.get("mode") == "shipyard", "unexpected daemon mode")
    _require(receipt.get("installed_sha256") == installed_sha256, "installed hash changed")
    _require(receipt.get("transition_path") is None, "transition already started")
    _require(receipt.get("production_quiesced") is False, "production was quiesced")
    _require(receipt.get("production_restored") is False, "production was restarted")
    _require(receipt.get("production_preserved") is False, "unexpected preservation claim")
    _require(receipt.get("mutation_fence_proved") is False, "unexpected mutation proof")
    _require(receipt.get("old_lifetime_lock_owned") is False, "lifetime lock was owned")

    mutation_probe_output = receipt.get("mutation_probe_output")
    expected_probe = str(canary_root / "unexpected-mutation-ran")
    _require(mutation_probe_output == expected_probe, "mutation probe path mismatch")

    return {
        "schema_version": 1,
        "reason": "production-queue-active",
        "active_runs": active_runs,
        "old_production_pid": pid,
        "old_production_start_time": start_time,
        "installed_sha256": installed_sha256,
        "mutation_probe_output": expected_probe,
    }


def validate_retained_reconciliation(
    receipt: Any,
    *,
    installed_sha256: str,
    canary_root: Path,
    lease_dir: Path,
) -> dict[str, Any]:
    required_fields = {
        "schema_version",
        "reason",
        "guardian_pid",
        "guardian_start_time",
        "lease_dir",
        "lease_device",
        "lease_inode",
        "lease_ctime_ns",
        "lease_generation",
        "prior_canary_root",
        "candidate_stopped",
        "production_quiesced",
        "production_restored",
        "transition_path",
        "mutation_fence_proved",
        "old_production_pid",
        "old_production_start_time",
        "installed_sha256",
        "configured_repos",
        "active_runs",
        "lease_removed",
    }
    missing = sorted(required_fields.difference(receipt))
    _require(not missing, f"missing required fields: {missing}")
    _require(SHA256.fullmatch(installed_sha256) is not None, "invalid expected hash")
    _require(canary_root.is_absolute() and lease_dir.is_absolute(), "paths must be absolute")
    _require(receipt.get("schema_version") == 1, "unsupported receipt schema")
    _require(receipt.get("reason") == RETAINED_RECONCILIATION_REASON, "wrong reason")
    _require(receipt.get("lease_dir") == str(lease_dir), "lease path mismatch")
    prior_root = receipt.get("prior_canary_root")
    _require(isinstance(prior_root, str), "invalid prior canary root")
    prior_path = Path(prior_root)
    lease_prefix = lease_dir.name
    if lease_prefix.endswith("-lease"):
        lease_prefix = lease_prefix[: -len("-lease")]
    _require(
        prior_path.parent == lease_dir.parent
        and prior_path.name.startswith(lease_prefix + "-"),
        "prior canary root is outside the lease namespace",
    )
    for field in (
        "guardian_pid",
        "old_production_pid",
        "lease_device",
        "lease_inode",
        "lease_ctime_ns",
    ):
        value = receipt.get(field)
        _require(
            isinstance(value, int) and not isinstance(value, bool) and value > 1,
            f"invalid {field}",
        )
    generation = receipt.get("lease_generation")
    _require(
        isinstance(generation, str)
        and len(generation) == 64
        and all(character in "0123456789abcdef" for character in generation),
        "invalid lease generation",
    )
    _require(
        isinstance(receipt.get("guardian_start_time"), str)
        and bool(receipt.get("guardian_start_time")),
        "invalid guardian start time",
    )
    _require(
        isinstance(receipt.get("old_production_start_time"), str)
        and bool(receipt.get("old_production_start_time")),
        "invalid production start time",
    )
    active_runs = receipt.get("active_runs")
    _require(
        isinstance(active_runs, list)
        and bool(active_runs)
        and all(isinstance(run, str) and bool(run) for run in active_runs),
        "active_runs must contain non-empty strings",
    )
    repos = receipt.get("configured_repos")
    _require(
        isinstance(repos, list)
        and all(isinstance(repo, str) and bool(repo) for repo in repos),
        "configured_repos must contain strings",
    )
    _require(receipt.get("installed_sha256") == installed_sha256, "installed hash changed")
    _require(receipt.get("candidate_stopped") is True, "candidate was not stopped")
    _require(receipt.get("production_quiesced") is False, "production was quiesced")
    _require(receipt.get("production_restored") is False, "production was restarted")
    _require(
        receipt.get("transition_path") == "corrected-idle-preserve-fence",
        "wrong transition",
    )
    _require(receipt.get("mutation_fence_proved") is True, "mutation fence missing")
    _require(receipt.get("lease_removed") is False, "retained lease already removed")
    return {
        "schema_version": 1,
        "reason": RETAINED_RECONCILIATION_REASON,
        "active_runs": active_runs,
        "guardian_pid": receipt["guardian_pid"],
        "guardian_start_time": receipt["guardian_start_time"],
        "old_production_pid": receipt["old_production_pid"],
        "old_production_start_time": receipt["old_production_start_time"],
        "installed_sha256": installed_sha256,
        "lease_dir": str(lease_dir),
        "lease_device": receipt["lease_device"],
        "lease_inode": receipt["lease_inode"],
        "lease_ctime_ns": receipt["lease_ctime_ns"],
        "lease_generation": generation,
        "prior_canary_root": prior_root,
    }


def validate_live_retained_lease(
    marker: Any, *, lease_dir: Path
) -> dict[str, Any]:
    """Revalidate the exact retained generation without trusting shell stat."""
    _require(isinstance(marker, dict), "deferral marker must be a JSON object")
    _require(
        marker.get("reason") == RETAINED_RECONCILIATION_REASON,
        "marker is not a retained reconciliation",
    )
    _require(lease_dir.is_absolute(), "lease path must be absolute")
    before = lease_dir.lstat()
    _require(
        stat.S_ISDIR(before.st_mode)
        and not stat.S_ISLNK(before.st_mode)
        and stat.S_IMODE(before.st_mode) == 0o700
        and before.st_uid == os.getuid(),
        "retained lease directory has unsafe metadata",
    )
    generation_path = lease_dir / LEASE_GENERATION_MARKER
    _require(
        tuple(lease_dir.iterdir()) == (generation_path,),
        "retained lease has unexpected contents",
    )
    generation_stat = generation_path.lstat()
    _require(
        stat.S_ISREG(generation_stat.st_mode)
        and not stat.S_ISLNK(generation_stat.st_mode)
        and stat.S_IMODE(generation_stat.st_mode) == 0o600
        and generation_stat.st_uid == os.getuid()
        and generation_stat.st_nlink == 1,
        "retained lease generation marker has unsafe metadata",
    )
    descriptor = os.open(generation_path, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        opened = os.fstat(descriptor)
        _require(
            (opened.st_dev, opened.st_ino)
            == (generation_stat.st_dev, generation_stat.st_ino),
            "retained lease generation marker changed while opening",
        )
        with os.fdopen(descriptor, "r", encoding="utf-8") as handle:
            descriptor = -1
            payload = handle.read(1024 * 1024 + 1)
    finally:
        if descriptor >= 0:
            os.close(descriptor)
    _require(len(payload) <= 1024 * 1024, "generation marker is oversized")
    try:
        generation = json.loads(payload)
    except json.JSONDecodeError as error:
        raise DeferralError(f"generation marker is invalid JSON: {error}") from error
    _require(
        isinstance(generation, dict)
        and generation.get("schema_version") == 1
        and generation.get("phase") == "transitioning",
        "generation marker is not a transitioning schema-1 lease",
    )
    after = lease_dir.lstat()
    observed_identity = (after.st_dev, after.st_ino, after.st_ctime_ns)
    _require(
        observed_identity == (before.st_dev, before.st_ino, before.st_ctime_ns),
        "retained lease changed during validation",
    )
    _require(
        observed_identity
        == (
            marker.get("lease_device"),
            marker.get("lease_inode"),
            marker.get("lease_ctime_ns"),
        ),
        "retained lease identity does not match deferral marker",
    )
    _require(
        generation.get("generation") == marker.get("lease_generation"),
        "retained lease generation does not match deferral marker",
    )
    return marker


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--receipt", type=Path)
    parser.add_argument("--installed-sha256")
    parser.add_argument("--canary-root", type=Path)
    parser.add_argument("--lease-dir", type=Path)
    parser.add_argument("--verify-live-marker", type=Path)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        if args.verify_live_marker is not None:
            _require(args.receipt is None, "choose receipt or live-marker validation")
            _require(args.lease_dir is not None, "live validation requires lease dir")
            marker = validate_live_retained_lease(
                json.loads(args.verify_live_marker.read_text(encoding="utf-8")),
                lease_dir=args.lease_dir,
            )
        else:
            _require(args.receipt is not None, "receipt is required")
            _require(args.installed_sha256 is not None, "installed hash is required")
            _require(args.canary_root is not None, "canary root is required")
            receipt = json.loads(args.receipt.read_text(encoding="utf-8"))
            marker = validate_deferral(
                receipt,
                installed_sha256=args.installed_sha256,
                canary_root=args.canary_root,
                lease_dir=args.lease_dir,
            )
    except (OSError, json.JSONDecodeError, DeferralError) as error:
        print(f"invalid sandbox admission deferral: {error}", file=sys.stderr)
        return 1
    json.dump(marker, sys.stdout, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

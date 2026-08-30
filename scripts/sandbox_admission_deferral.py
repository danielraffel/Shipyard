#!/usr/bin/env python3
"""Validate a guardian receipt that safely deferred a production-host canary."""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from pathlib import Path
from typing import Any


ACTIVE_WORKER_FAILURE = "GuardianError: refusing canary transition with active workers:"
SHA256 = re.compile(r"[0-9a-f]{64}\Z")


class DeferralError(ValueError):
    """The receipt does not prove a safe admission deferral."""


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise DeferralError(message)


def validate_deferral(
    receipt: Any, *, installed_sha256: str, canary_root: Path
) -> dict[str, Any]:
    _require(isinstance(receipt, dict), "receipt must be a JSON object")
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


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--installed-sha256", required=True)
    parser.add_argument("--canary-root", type=Path, required=True)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        receipt = json.loads(args.receipt.read_text(encoding="utf-8"))
        marker = validate_deferral(
            receipt,
            installed_sha256=args.installed_sha256,
            canary_root=args.canary_root,
        )
    except (OSError, json.JSONDecodeError, DeferralError) as error:
        print(f"invalid sandbox admission deferral: {error}", file=sys.stderr)
        return 1
    json.dump(marker, sys.stdout, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

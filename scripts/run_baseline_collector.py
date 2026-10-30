#!/usr/bin/env python3
"""Run baseline collection with an optional fresh merge-receipt handoff."""
from __future__ import annotations

import argparse
import os
import subprocess
import sys


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--collector", required=True)
    parser.add_argument("--handoff-script", required=True)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--base-branch", default="main")
    parser.add_argument("--merge-receipts", required=True)
    parser.add_argument("--merge-receipt-health", required=True)
    parser.add_argument("collector_args", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.collector_args[:1] == ["--"]:
        args.collector_args = args.collector_args[1:]
    check = subprocess.run(
        [sys.executable, args.handoff_script, "check", "--file", args.merge_receipts,
         "--repo", args.repo, "--base-branch", args.base_branch,
         "--health-file", args.merge_receipt_health, "--stale-after", "2h"],
        capture_output=True, text=True, check=False,
    )
    collector_args = list(args.collector_args)
    if check.returncode == 0:
        collector_args.extend([
            "--merge-receipts", args.merge_receipts,
            "--merge-receipt-health", args.merge_receipt_health,
            "--merge-receipt-base-branch", args.base_branch,
        ])
    else:
        detail = (check.stderr or check.stdout or "handoff validation failed").strip()
        collector_args.extend(["--merge-receipt-unavailable-reason", " ".join(detail.split())[:500]])
    os.execv(sys.executable, [sys.executable, args.collector, *collector_args])
    return 127


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Acceptance check: the scorecard must reproduce six numbers measured by hand on 2026-11-15, within a tolerance.

The hand-measured values come from the DORA design draft (acceptance test before replacing anything). Each is
compared with the same quantity read from the scorecard's own per-PR rows over PRs merged in the window:

  ready-to-merge median (Vellum, Pulp)       handoff_min.median
  Vellum PR-level GPU proofs per merged PR   by_workflow["Locked macOS GPU proof"].pull_request
  Vellum merge-group proofs per merged PR    by_workflow["Locked macOS GPU proof"].merge_group
  Pulp macOS minutes per merged PR           macos_min_mean
  Vellum enqueue-to-merge median             merge_queue.enqueue_to_merge_min.median

    acceptance.py --from 2026-11-14 --to 2026-11-16 --cache DIR [--tolerance 0.15] [--json-out F]

Reads GitHub through ghread/ghapp (shared read budget) and caches per-PR facts under --cache.
"""
from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import sys

UTC = dt.timezone.utc
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import pr_metrics  # noqa: E402

PROOF = "Locked macOS GPU proof"
# name, repo, hand-measured value, unit, how to read it from a pr_metrics.summarize() dict
ACCEPTANCE = [
    ("Vellum ready-to-merge median", "Generous-Corp/vellum", 365.0, "min", lambda s: s["handoff_min"]["median"]),
    ("Pulp ready-to-merge median", "Generous-Corp/pulp", 92.0, "min", lambda s: s["handoff_min"]["median"]),
    ("Vellum PR GPU proofs per merged PR", "Generous-Corp/vellum", 2.9, "runs",
     lambda s: s["compute_per_merged_pr"]["by_workflow"].get(PROOF, {}).get("pull_request")),
    ("Vellum merge-group proofs per merged PR", "Generous-Corp/vellum", 0.87, "runs",
     lambda s: s["compute_per_merged_pr"]["by_workflow"].get(PROOF, {}).get("merge_group")),
    ("Pulp macOS minutes per merged PR", "Generous-Corp/pulp", 69.0, "min", lambda s: s["compute_per_merged_pr"]["macos_min_mean"]),
    ("Vellum enqueue-to-merge median", "Generous-Corp/vellum", 54.0, "min", lambda s: s["merge_queue"]["enqueue_to_merge_min"]["median"]),
]


def compare(summaries, tolerance=0.15):
    """summaries: repo -> pr_metrics.summarize() over the window. Returns rows with value, delta and verdict."""
    out = []
    for name, repo, hand, unit, read in ACCEPTANCE:
        s = summaries.get(repo)
        try:
            got = None if s is None else read(s)
        except (KeyError, TypeError):
            got = None
        delta = None if got is None else round((got - hand) / hand * 100.0, 1)
        out.append({"name": name, "repo": repo, "hand_measured": hand, "unit": unit, "report": got, "delta_pct": delta,
                    "within": delta is not None and abs(delta) <= tolerance * 100.0})
    return out


def table(rows, tolerance):
    L = [f"| number | hand-measured | report | delta | within {int(tolerance * 100)}% |", "|---|---|---|---|---|"]
    for r in rows:
        L.append(f"| {r['name']} | {r['hand_measured']} {r['unit']} | {r['report']} | {'n/a' if r['delta_pct'] is None else str(r['delta_pct']) + '%'} | {'yes' if r['within'] else 'NO'} |")
    return "\n".join(L)


def measure(start, end, cache, cap, tolerance, fetch=None):
    """Read PRs merged in [start, end) from the two repos and return (rows, per-repo summaries, per-repo reads)."""
    repos = sorted({repo for _, repo, *_ in ACCEPTANCE})
    now = end + dt.timedelta(days=3)
    lo = start - dt.timedelta(days=2)  # PRs created up to two days before the window can merge in it
    summaries, meta = {}, {}
    for repo in repos:
        res = pr_metrics.collect_repo(repo, cache, cap, "danielraffel", lo, now, now, {}, {}, fetch, "main", (), (start, end))
        rows = [r for r in res["rows"] if r["eligible"] and r["merged"]]
        summaries[repo] = pr_metrics.summarize(rows)
        meta[repo] = {"merged_prs": len(rows), "unread": res["unread"], "truncated": res["truncated"], "errors": res["errors"],
                      "compute_read": summaries[repo]["compute_per_merged_pr"]["prs_read"]}
    return compare(summaries, tolerance), summaries, meta


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--from", dest="start", required=True, help="first UTC day of PR merges, YYYY-MM-DD")
    ap.add_argument("--to", required=True, help="UTC day after the last day of merges, YYYY-MM-DD")
    ap.add_argument("--cache", required=True)
    ap.add_argument("--read-cap", type=int, default=12000)
    ap.add_argument("--tolerance", type=float, default=0.15)
    ap.add_argument("--json-out")
    a = ap.parse_args(argv)
    day = lambda x: dt.datetime.strptime(x, "%Y-%m-%d").replace(tzinfo=UTC)
    rows, summaries, meta = measure(day(a.start), day(a.to), a.cache, a.read_cap, a.tolerance)
    print(f"PRs merged {a.start} to {a.to} (UTC), eligible PRs only; reads per repo: {json.dumps(meta)}\n")
    print(table(rows, a.tolerance))
    if a.json_out:
        with open(a.json_out, "w") as fh:
            json.dump({"rows": rows, "meta": meta, "summaries": summaries}, fh, indent=1)
    return 0 if all(r["within"] for r in rows) else 1


if __name__ == "__main__":
    sys.exit(main())

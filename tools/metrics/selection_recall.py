#!/usr/bin/env python3
"""Selection recall for a window: how often would narrowing the merge-group gate have missed a failing test.

Run as `python3 -I tools/metrics/selection_recall.py --repo OWNER/REPO --since 2026-11-11 --until 2026-11-17`.
Standard library only. Read-only: every call is a GitHub GET, and the report files are the only writes.
Definitions live in docs/metrics.md ("Selection recall report").

  selection_recall.py --repo R --since D --until D [--workflow .github/workflows/build.yml] [--job macos]
                      [--runner-class fleet|hosted|any] [--cache DIR] [--read-cap N] [--workers N]
                      [--json-out F] [--md-out F]

For each completed merge-group gate job the tool reads the check-run annotations and parses the shadow notices a
planner emits: `affected-tests-shadow` or `shipyard-selection-shadow` (what it would have selected and which failing
tests fell outside it), `test-receipts-shadow` (what receipts would have skipped) and `flake-exoneration` (whether a
failing test is a known flake). A job whose selection notice is missing or unparseable is unread and keeps the result
`partial`; so does anything else that could not be read.
"""
from __future__ import annotations

import argparse
import collections
import datetime as dt
import json
import math
import os
import re
import sys
import threading
from concurrent.futures import ThreadPoolExecutor

SCHEMA = "shipyard-selection-recall/v1"
UTC = dt.timezone.utc
SELECTION_TITLES = ("affected-tests-shadow", "shipyard-selection-shadow")
RECEIPTS_TITLE = "test-receipts-shadow"
EXONERATION_TITLES = ("flake-exoneration", "flake-exoneration-shadow")  # the first says `excused`, the second `would_exonerate`
GATE_CONCLUSIONS = ("success", "failure")
COVERAGE_FLOOR = 0.95
DEFAULT_READ_CAP = 3000
LIST_PAGES = 10
JOB_PAGES = 5
RUN_FIELDS = ("id", "name", "path", "event", "status", "conclusion", "head_sha", "created_at", "run_attempt")
JOB_FIELDS = ("id", "name", "status", "conclusion", "labels", "runner_name", "run_attempt")


def _pr_metrics():
    """The sibling module. `python3 -I` leaves the script's own directory off sys.path, so add it."""
    here = os.path.dirname(os.path.abspath(__file__))
    if here not in sys.path:
        sys.path.insert(0, here)
    import pr_metrics
    return pr_metrics


pm = _pr_metrics()


class RecallReader(pm.Reader):
    """The scorecard's Reader with a lock on its counters, so a few threads can share one read budget."""

    def __init__(self, *a, **kw):
        super().__init__(*a, **kw)
        self._lock = threading.Lock()

    def get(self, path):
        with self._lock:
            if self.calls >= self.cap:
                self.truncated = True
                raise pm.Truncated()
            self.calls += 1
        try:
            return self.fetch(f"repos/{self.repo}/{path}")
        except pm.ReadError:
            with self._lock:
                self.errors += 1
            raise


def pct(vals, p):
    """Linear-interpolated percentile of a non-empty list, p in 0..100."""
    v = sorted(vals)
    if len(v) == 1:
        return v[0]
    k = (len(v) - 1) * p / 100.0
    lo, hi = math.floor(k), math.ceil(k)
    return v[lo] + (v[hi] - v[lo]) * (k - lo)


def runner_class(labels):
    return "fleet" if any(str(x).lower() == "self-hosted" for x in (labels or [])) else "hosted"


def days(since, until):
    d = since
    while d <= until:
        yield d
        d += dt.timedelta(days=1)


# ------------------------------------------------------------------ reading
def list_runs(rd, workflow, day):
    """Merge-group runs of one workflow created on UTC `day`. Returns (runs, complete)."""
    wf = os.path.basename(workflow)
    q = (f"actions/workflows/{wf}/runs?event=merge_group"
         f"&created={day}T00:00:00Z..{day}T23:59:59Z")
    runs, total = [], None
    for page in range(1, LIST_PAGES + 1):
        d = rd.get(f"{q}&per_page=100&page={page}")
        items = d.get("workflow_runs") if isinstance(d, dict) else None
        if not isinstance(items, list):
            raise pm.ReadError("unexpected shape")
        total = d.get("total_count")
        runs += [{k: x.get(k) for k in RUN_FIELDS} for x in items]
        if len(items) < 100:
            break
    return runs, total is None or len(runs) >= total


def run_jobs(rd, run):
    """(jobs, complete) of a run's latest attempt. A completed run's jobs are cached with the attempt they saw."""
    cached = rd.load("gate-jobs", str(run["id"]))
    if isinstance(cached, dict) and cached.get("attempt") == (run.get("run_attempt") or 1):
        return cached["jobs"], True
    items, complete = rd.pages(f"actions/runs/{run['id']}/jobs?filter=latest", key="jobs", max_pages=JOB_PAGES)
    jobs = [{k: x.get(k) for k in JOB_FIELDS} for x in items]
    if complete and run.get("status") == "completed":
        rd.save("gate-jobs", str(run["id"]), {"attempt": run.get("run_attempt") or 1, "jobs": jobs})
    return jobs, complete


def job_annotations(rd, job_id):
    """(annotations, complete) of one check run. A finished job's annotations never change, so they are cached."""
    cached = rd.load("annotations", str(job_id))
    if cached is not None:
        return cached, True
    items, complete = rd.pages(f"check-runs/{job_id}/annotations", max_pages=3)
    ann = [{"title": a.get("title") or "", "message": a.get("message") or ""} for a in items]
    if complete:
        rd.save("annotations", str(job_id), ann)
    return ann, complete


def collect(rd, since, until, workflow, job_name, runner, workers=4):
    """Read the gate jobs and their annotations. Returns the data `compute` takes."""
    partial, runs = [], []
    for d in days(since, until):
        try:
            got, complete = list_runs(rd, workflow, d)
        except (pm.Truncated, pm.ReadError) as e:
            partial.append(f"listing of merge-group runs on {d} not read ({type(e).__name__})")
            continue
        if not complete:
            partial.append(f"listing of merge-group runs on {d} truncated at {len(got)}")
        runs += got
    done = [r for r in runs if r["status"] == "completed"]
    skipped = {"run_not_completed": len(runs) - len(done), "not_gate_job": 0, "other_runner_class": 0,
               "cancelled_or_skipped": 0}

    def jobs_of(r):
        try:
            jobs, complete = run_jobs(rd, r)
        except (pm.Truncated, pm.ReadError):
            return r, None
        return r, (jobs if complete else None)

    with ThreadPoolExecutor(max(1, workers)) as ex:
        per_run = list(ex.map(jobs_of, done))
    gate = []
    unread_runs = 0
    for r, jobs in per_run:
        if jobs is None:
            unread_runs += 1
            continue
        for j in jobs:
            if j["name"] != job_name:
                skipped["not_gate_job"] += 1
            elif runner != "any" and runner_class(j.get("labels")) != runner:
                skipped["other_runner_class"] += 1
            elif j["status"] != "completed" or j["conclusion"] not in GATE_CONCLUSIONS:
                skipped["cancelled_or_skipped"] += 1
            else:
                gate.append({"job_id": j["id"], "run_id": r["id"], "head_sha": r["head_sha"],
                             "conclusion": j["conclusion"]})
    if unread_runs:
        partial.append(f"{unread_runs} of {len(done)} runs' jobs not read in full")

    def ann_of(g):
        try:
            a, complete = job_annotations(rd, g["job_id"])
        except (pm.Truncated, pm.ReadError):
            return g["job_id"], None
        return g["job_id"], (a if complete else None)

    with ThreadPoolExecutor(max(1, workers)) as ex:
        annotations = dict(ex.map(ann_of, gate))
    if rd.truncated:
        partial.append(f"read cap of {rd.cap} reached")
    return {"gate": gate, "annotations": annotations, "partial": partial, "skipped": skipped,
            "reads": rd.calls, "errors": rd.errors}


# ------------------------------------------------------------------ calculation
def parse_notice(a):
    """The JSON object in an annotation's message, or None."""
    try:
        v = json.loads(a["message"])
    except ValueError:
        return None
    return v if isinstance(v, dict) else None


def normalize_reason(reason):
    return re.sub(r"^\d+ ", "N ", reason or "unknown")


def _count(v):
    """A non-negative integer as JSON wrote it, or None. A bool, a string or a float is not a count."""
    return v if isinstance(v, int) and not isinstance(v, bool) and v >= 0 else None


def read_job(g, annotations):
    """One gate job's facts, or (None, why) when its selection notice is missing or unusable."""
    if annotations is None:
        return None, "annotations not read"
    sel = [a for a in annotations if a["title"] in SELECTION_TITLES]
    if not sel:
        return None, "no selection annotation"
    s = parse_notice(sel[0])
    if s is None:
        return None, "selection annotation unparseable"
    total, selected = _count(s.get("total")), _count(s.get("selected"))
    failed, outside = _count(s.get("failed", 0)), _count(s.get("failed_outside_selection", 0))
    names = s.get("failed_outside_names") or []
    select_all = s.get("select_all")
    if (None in (total, selected, failed, outside) or not isinstance(select_all, bool) or selected > total
            or outside > failed or not isinstance(names, list) or not all(isinstance(n, str) for n in names)):
        return None, "selection annotation invalid"
    f = {"job_id": g["job_id"], "run_id": g["run_id"], "conclusion": g["conclusion"], "select_all": select_all,
         "reason": s.get("reason"), "total": total, "selected": selected, "failed": failed, "outside": outside,
         "outside_names": names, "fraction": (selected / total) if total else None,
         "receipts": None, "exoneration": {}}
    for a in annotations:
        if a["title"] == RECEIPTS_TITLE:
            r = parse_notice(a)
            skip, sel_n, bad = _count(r.get("would_skip") if r else None), _count(r.get("selected") if r else None), \
                _count(r.get("would_skip_failed", 0) if r else None)
            names = (r or {}).get("would_skip_failed_names") or []
            if None in (skip, sel_n, bad) or skip > sel_n or not isinstance(names, list):
                return None, "receipts annotation invalid"
            f["receipts"] = {"would_skip": skip, "selected": sel_n, "would_skip_failed": bad,
                             "failed_names": [str(n) for n in names]}
        elif a["title"] in EXONERATION_TITLES:
            e = parse_notice(a)
            if e and isinstance(e.get("tests"), dict):
                f["exoneration"].update(e["tests"])
    return f, None


def verdict(f, name):
    t = f["exoneration"].get(name)
    if not isinstance(t, dict):
        return "no verdict"
    if t.get("excused") or t.get("would_exonerate"):
        return "exonerated"
    return f"not excused: {t.get('reason') or 'unknown'}"


def compute(data, repo, since, until, workflow, job_name, runner="fleet"):
    """The recall report from collected data. Pure: no reads."""
    jobs, unread, unread_jobs = [], collections.Counter(), []
    for g in data["gate"]:
        f, why = read_job(g, data["annotations"].get(g["job_id"]))
        if f:
            jobs.append(f)
        else:
            unread[why] += 1
            unread_jobs.append({"job_id": g["job_id"], "run_id": g["run_id"], "conclusion": g["conclusion"], "why": why})
    n_gate = len(data["gate"])
    narrowed = [f for f in jobs if not f["select_all"]]
    select_all = [f for f in jobs if f["select_all"]]
    reasons = collections.Counter(normalize_reason(f["reason"]) for f in select_all)
    fr = [f["fraction"] * 100 for f in narrowed if f["fraction"] is not None]
    misses = [{"job_id": f["job_id"], "run_id": f["run_id"], "test": n, "verdict": verdict(f, n)}
              for f in narrowed for n in f["outside_names"]]
    miss_count = sum(f["outside"] for f in narrowed)
    # `outside` counts tests; the list names them. A miss the job did not name cannot be judged, so it is unexplained.
    exonerated = sum(1 for m in misses if m["verdict"] == "exonerated")
    unexplained = max(0, miss_count - exonerated)
    rcpt = [f["receipts"] for f in jobs if f["receipts"]]
    coverage = (len(jobs) / n_gate) if n_gate else None
    reasons_partial = list(data["partial"])
    n_unread = sum(unread.values())
    if n_unread:
        reasons_partial.append(f"{n_unread} of {n_gate} gate jobs unread: " +
                               ", ".join(f"{v} {k}" for k, v in unread.items()))
    if coverage is not None and coverage < COVERAGE_FLOOR:
        reasons_partial.append(f"only {coverage:.1%} of gate jobs carried a selection annotation (floor {COVERAGE_FLOOR:.0%})")
    return {
        "schema": SCHEMA, "repo": repo, "since": since.isoformat(), "until": until.isoformat(),
        "workflow": workflow, "job": job_name, "runner_class": runner,
        "partial": bool(reasons_partial), "partial_reasons": reasons_partial,
        "reads": data["reads"], "read_errors": data["errors"], "excluded": data["skipped"],
        "gate_jobs": n_gate, "jobs_read": len(jobs), "jobs_unread": n_unread, "unread_jobs": unread_jobs, "coverage": None if coverage is None else round(coverage, 4),
        "select_all": {"jobs": len(select_all), "share": _share(len(select_all), len(jobs)),
                       "reasons": [{"reason": k, "jobs": v, "share": _share(v, len(jobs))} for k, v in reasons.most_common()]},
        "narrowed": {"jobs": len(narrowed), "share": _share(len(narrowed), len(jobs)),
                     "selected_fraction_median_pct": round(pct(fr, 50), 2) if fr else None,
                     "selected_fraction_p90_pct": round(pct(fr, 90), 2) if fr else None,
                     "failing_tests": sum(f["failed"] for f in narrowed), "misses": miss_count,
                     "misses_per_100_narrowed": round(miss_count / len(narrowed) * 100, 1) if narrowed else None,
                     "exonerated_misses": exonerated, "unexplained_misses": unexplained,
                     "unexplained_per_100_narrowed": round(unexplained / len(narrowed) * 100, 1) if narrowed else None,
                     "miss_list": misses},
        "failures": {"jobs_with_failing_test": sum(1 for f in jobs if f["failed"]),
                     "failing_tests": sum(f["failed"] for f in jobs)},
        "receipts": {"jobs": len(rcpt),
                     "would_skip_median_pct": round(pct([r["would_skip"] / r["selected"] * 100 for r in rcpt if r["selected"]], 50), 2)
                     if any(r["selected"] for r in rcpt) else None,
                     "would_skip_failed": sum(r["would_skip_failed"] for r in rcpt),
                     "would_skip_failed_names": sorted({n for r in rcpt for n in r["failed_names"]})},
    }


def _share(n, d):
    return round(n / d, 4) if d else None


# ------------------------------------------------------------------ output
def pc(x):
    return "n/a" if x is None else f"{x:.1%}"


def render_md(r):
    out = [f"# Selection recall: {r['repo']}, {r['since']} to {r['until']}", ""]
    if r["partial"]:
        out += ["**PARTIAL.** Not every gate job was read:"] + [f"- {x}" for x in r["partial_reasons"]] + [""]
    n, s, f = r["narrowed"], r["select_all"], r["failures"]
    out += [f"Workflow `{r['workflow']}`, job `{r['job']}` ({r['runner_class']} runners). Reads: {r['reads']}. "
            f"Excluded jobs: {r['excluded']}.", "",
            "| Measure | Value |", "|---|---|",
            f"| Gate jobs / read / unread | {r['gate_jobs']} / {r['jobs_read']} / {r['jobs_unread']} |",
            f"| Select-all | {s['jobs']} ({pc(s['share'])}) |",
            f"| Narrowed | {n['jobs']} ({pc(n['share'])}) |",
            f"| Selected share in a narrowed job | median {n['selected_fraction_median_pct']}%, p90 {n['selected_fraction_p90_pct']}% |",
            f"| Gate jobs with a failing test / failing tests | {f['jobs_with_failing_test']} / {f['failing_tests']} |",
            f"| Failing tests in narrowed jobs | {n['failing_tests']} |",
            f"| Misses (failed outside the selection) | {n['misses']} |",
            f"| **Misses per 100 narrowed jobs** | **{n['misses_per_100_narrowed']}** |",
            f"| Exonerated misses / unexplained misses | {n['exonerated_misses']} / {n['unexplained_misses']} |",
            f"| **Unexplained misses per 100 narrowed jobs** | **{n['unexplained_per_100_narrowed']}** |",
            f"| Receipts: jobs with the notice, median share it would skip | {r['receipts']['jobs']}, {r['receipts']['would_skip_median_pct']}% |",
            f"| Receipts: skipped tests that failed | {r['receipts']['would_skip_failed']} {r['receipts']['would_skip_failed_names']} |",
            "", "## Select-all reasons", "", "| Reason | Jobs | Share of read jobs |", "|---|---|---|"]
    out += [f"| {x['reason']} | {x['jobs']} | {pc(x['share'])} |" for x in s["reasons"]]
    out += ["", "## Misses", "", "| Job | Test | Exoneration verdict |", "|---|---|---|"]
    out += [f"| {m['job_id']} | {m['test']} | {m['verdict']} |" for m in n["miss_list"]]
    if r["unread_jobs"]:
        out += ["", "## Unread gate jobs", "", "| Job | Run | Conclusion | Why |", "|---|---|---|---|"]
        out += [f"| {u['job_id']} | {u['run_id']} | {u['conclusion']} | {u['why']} |" for u in r["unread_jobs"][:50]]
    return "\n".join(out) + "\n"


def main(argv=None, fetch=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--repo", required=True)
    ap.add_argument("--since", required=True, help="first UTC day, YYYY-MM-DD")
    ap.add_argument("--until", required=True, help="last UTC day, inclusive")
    ap.add_argument("--workflow", default=".github/workflows/build.yml")
    ap.add_argument("--job", default="macos")
    ap.add_argument("--runner-class", default="fleet", choices=("fleet", "hosted", "any"),
                    help="only jobs on this runner class are gate jobs (default fleet: a hosted job of the same name "
                         "is the bootstrap and reuse stub, which runs no suite and emits no notice)")
    ap.add_argument("--cache", default=os.path.join(os.path.expanduser("~"), ".cache", "shipyard-selection-recall"))
    ap.add_argument("--read-cap", type=int, default=DEFAULT_READ_CAP)
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--quiet", action="store_true", help="do not print the report")
    ap.add_argument("--json-out")
    ap.add_argument("--md-out")
    a = ap.parse_args(argv)
    since, until = dt.date.fromisoformat(a.since), dt.date.fromisoformat(a.until)
    if until < since:
        ap.error("bad window")
    rd = RecallReader(a.repo, a.cache, a.read_cap, fetch=fetch)
    data = collect(rd, since, until, a.workflow, a.job, a.runner_class, a.workers)
    rep = compute(data, a.repo, since, until, a.workflow, a.job, a.runner_class)
    if a.json_out:
        with open(a.json_out, "w") as fh:
            json.dump(rep, fh, indent=1)
    md = render_md(rep)
    if a.md_out:
        with open(a.md_out, "w") as fh:
            fh.write(md)
    if not a.quiet:
        print(md)
    return 0


if __name__ == "__main__":
    sys.exit(main())

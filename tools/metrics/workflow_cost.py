#!/usr/bin/env python3
"""Per-workflow cost, queue, duplicate, short-job wait and never-landed report for one repository and window.

Run as `python3 -I tools/metrics/workflow_cost.py --repo OWNER/REPO --since 2026-11-11 --until 2026-11-17`.
Standard library only. Read-only: every call is a GitHub GET, and the report files are the only writes.
Definitions live in docs/metrics.md ("Workflow cost report").

  workflow_cost.py --repo R --since D --until D [--events push,merge_group,pull_request]
                   [--workflow NAME_OR_PATH ...] [--branch main] [--cache DIR] [--read-cap N]
                   [--workers N] [--json-out F] [--md-out F]

A truncated listing (a day with more than 1,000 runs for one event), a run whose jobs were not read in full, a read
cap hit or a read error is `partial`: it is named in the report and never counted as zero.
"""
from __future__ import annotations

import argparse
import collections
import datetime as dt
import json
import math
import os
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor

SCHEMA = "shipyard-workflow-cost/v1"
UTC = dt.timezone.utc
SHORT_JOB_MINUTES = 3.0
LAND_HORIZON_DAYS = 7  # how long after the window a merge-group SHA may still land and count as landed
LIST_TTL_SECONDS = 24 * 3600  # a cached day listing is read again after this, so a later re-run shows up
LIST_PAGES = 10  # the search cap is 1,000 results per query, 100 a page
JOB_PAGES = 5
DEFAULT_EVENTS = ("push", "merge_group", "pull_request")
DEFAULT_READ_CAP = 8000
GENERIC_LABELS = {"self-hosted", "macos", "linux", "windows", "arm64", "x64", "x86", "x86_64", "aarch64"}
RUN_FIELDS = ("id", "name", "path", "event", "status", "conclusion", "head_sha", "head_branch", "created_at",
              "run_attempt")
JOB_FIELDS = ("id", "name", "status", "conclusion", "created_at", "started_at", "completed_at", "labels",
              "runner_name", "run_attempt")


def _pr_metrics():
    """The sibling module. `python3 -I` leaves the script's own directory off sys.path, so add it."""
    here = os.path.dirname(os.path.abspath(__file__))
    if here not in sys.path:
        sys.path.insert(0, here)
    import pr_metrics
    return pr_metrics


pm = _pr_metrics()


class CostReader(pm.Reader):
    """The scorecard's Reader (cache, per-repo cap, ghread or ghapp by owner) with a lock on its counters,
    so a few threads can share one budget."""

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


def parse_time(s):
    return dt.datetime.fromisoformat(s.replace("Z", "+00:00")) if s else None


def pct(vals, p):
    """Linear-interpolated percentile of a non-empty list, p in 0..100."""
    v = sorted(vals)
    if len(v) == 1:
        return v[0]
    k = (len(v) - 1) * p / 100.0
    lo, hi = math.floor(k), math.ceil(k)
    return v[lo] + (v[hi] - v[lo]) * (k - lo)


def runner_class(labels):
    """("fleet", first repo-specific label) when `self-hosted` is a label, else ("hosted", first label)."""
    low = [str(x) for x in (labels or [])]
    if any(x.lower() == "self-hosted" for x in low):
        own = [x for x in low if x.lower() not in GENERIC_LABELS]
        return "fleet", (own[0] if own else "self-hosted")
    return "hosted", (low[0] if low else "none")


def job_cost(job):
    """(minutes, queue_minutes) of one job, or (0.0, None) when it never ran: skipped, or never given a runner."""
    if job.get("conclusion") == "skipped" or not job.get("runner_name"):
        return 0.0, None
    s, e, c = parse_time(job.get("started_at")), parse_time(job.get("completed_at")), parse_time(job.get("created_at"))
    if not s or not e:
        return 0.0, None
    queue = max(0.0, (s - c).total_seconds() / 60) if c else None
    return max(0.0, (e - s).total_seconds() / 60), queue


def days(since, until):
    d = since
    while d <= until:
        yield d
        d += dt.timedelta(days=1)


# ------------------------------------------------------------------ reading
def list_range(rd, event, lo, hi, branch):
    """Runs of `event` created in [lo, hi] (datetimes). The search returns at most 1,000 results, so a range whose
    total is larger is split in half and each half read, down to one second. Returns (runs, complete)."""
    q = f"actions/runs?event={event}&created={lo:%Y-%m-%dT%H:%M:%SZ}..{hi:%Y-%m-%dT%H:%M:%SZ}"
    if branch:
        q += f"&branch={branch}"
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
    if total is None or len(runs) >= total:
        return runs, True
    if hi - lo <= dt.timedelta(seconds=1):
        return runs, False
    mid = lo + (hi - lo) / 2
    mid = mid.replace(microsecond=0)
    a, ca = list_range(rd, event, lo, mid, branch)
    b, cb = list_range(rd, event, mid + dt.timedelta(seconds=1), hi, branch)
    return a + b, ca and cb


def fresh(rd, kind, name):
    try:
        age = time.time() - os.path.getmtime(os.path.join(rd.root, kind, f"{name}.json"))
    except OSError:
        return False
    return age < LIST_TTL_SECONDS


def list_day(rd, event, day, branch, today):
    """Every run of `event` created on UTC `day`. Returns (runs, complete). A day is cached for good once it is two
    or more days old and every run in it has completed."""
    key = f"{event}-{branch or 'all'}-{day.isoformat()}"
    cached = rd.load("list", key)
    if cached is not None and fresh(rd, "list", key):
        return cached, True
    lo = dt.datetime(day.year, day.month, day.day, tzinfo=UTC)
    runs, complete = list_range(rd, event, lo, lo + dt.timedelta(hours=24, seconds=-1), branch)
    if complete and (today - day).days >= 2 and all(r["status"] == "completed" for r in runs):
        rd.save("list", key, runs)
    return runs, complete


def run_jobs(rd, run):
    """(jobs, complete) for one run, every attempt. A completed run's jobs are cached for good."""
    cached = rd.load("jobs", str(run["id"]))
    # A re-run keeps the run id and adds attempts: a cache that has not seen the latest attempt is stale.
    if cached is not None and max([j.get("run_attempt") or 1 for j in cached] or [1]) >= (run.get("run_attempt") or 1):
        return cached, True
    items, complete = rd.pages(f"actions/runs/{run['id']}/jobs?filter=all", key="jobs", max_pages=JOB_PAGES)
    jobs = [{k: x.get(k) for k in JOB_FIELDS} for x in items]
    if complete and run.get("status") == "completed":
        rd.save("jobs", str(run["id"]), jobs)
    return jobs, complete


def wanted(run, workflows):
    if not workflows:
        return True
    return any(w == run["name"] or w == run["path"] or (run["path"] or "").endswith(w) for w in workflows)


def collect(rd, since, until, events, workflows, branch, today, workers=4):
    """Read the runs and jobs the report needs. Returns the data `compute` takes; reasons for partial are in
    data["partial"]."""
    partial = []
    plan = {(ev, d): True for ev in events for d in days(since, until)}
    if "push" in events or "merge_group" in events:
        # Context for duplicates and never-landed: a merge group is built before its push, so look one day back
        # for merge groups and LAND_HORIZON_DAYS ahead (up to today) for pushes. Listed only; their jobs are not read.
        ctx_days = [("merge_group", d) for d in days(since - dt.timedelta(days=1), until)]
        ctx_days += [("push", d) for d in days(since, min(until + dt.timedelta(days=LAND_HORIZON_DAYS), today))]
        for key in ctx_days:
            plan.setdefault(key, False)
    plan = [(ev, d, w) for (ev, d), w in plan.items()]
    runs = {ev: [] for ev in ("push", "merge_group", "pull_request")}
    ctx = {"push": [], "merge_group": []}
    for ev, d, in_win in plan:
        try:
            got, complete = list_day(rd, ev, d, branch if ev == "push" else None, today)
        except (pm.Truncated, pm.ReadError) as e:
            partial.append(f"listing of {ev} runs on {d} not read ({type(e).__name__})")
            continue
        if not complete:
            partial.append(f"listing of {ev} runs on {d} truncated at {len(got)}")
        if in_win:
            runs[ev] += got
        if ev in ctx:
            ctx[ev] += got
    todo, seen = [], set()
    for ev in events:
        for r in runs[ev]:
            if wanted(r, workflows) and r["id"] not in seen:
                seen.add(r["id"])
                todo.append(r)

    def one(r):
        try:
            jobs, complete = run_jobs(rd, r)
        except (pm.Truncated, pm.ReadError):
            return r["id"], None
        return r["id"], (jobs if complete else None)

    with ThreadPoolExecutor(max(1, workers)) as ex:
        jobs = dict(ex.map(one, todo))
    unread = sum(1 for v in jobs.values() if v is None)
    if unread:
        partial.append(f"{unread} of {len(todo)} runs' jobs not read in full")
    if rd.truncated:
        partial.append(f"read cap of {rd.cap} reached")
    return {"runs": {ev: [r for r in runs[ev] if r["id"] in seen] for ev in events}, "ctx": ctx, "jobs": jobs,
            "partial": partial, "reads": rd.calls, "errors": rd.errors}


# ------------------------------------------------------------------ calculation
def stats(vals):
    if not vals:
        return {"n": 0, "median": None, "p90": None, "sum": 0.0}
    return {"n": len(vals), "median": round(pct(vals, 50), 2), "p90": round(pct(vals, 90), 2),
            "sum": round(sum(vals), 2)}


def compute(data, repo, since, until, events, branch="main"):
    """The report from collected data: per workflow and event cost and queue, duplicates, short-job wait and
    never-landed minutes. Pure: no reads."""
    push_shas = {r["head_sha"] for r in data["ctx"]["push"] if r["head_branch"] == branch or not branch}
    mg_success = {(r["path"], r["head_sha"]) for r in data["ctx"]["merge_group"] if r["conclusion"] == "success"}
    rows, by_job, short = {}, {}, {}
    dup = collections.defaultdict(lambda: {"push_runs": 0, "same_sha_merge_group_success": 0})
    never = collections.defaultdict(lambda: {"runs": 0, "fleet_minutes": 0.0, "runs_not_landed": 0,
                                             "fleet_minutes_not_landed": 0.0, "in_flight": 0})
    for ev in events:
        for run in data["runs"].get(ev, []):
            jobs = data["jobs"].get(run["id"])
            wk = (run["name"], run["path"], ev)
            w = rows.setdefault(wk, {"workflow": run["name"], "path": run["path"], "event": ev, "runs": 0,
                                     "conclusions": collections.Counter(), "runs_unread": 0, "runs_unfinished": 0,
                                     "fleet_minutes": 0.0, "hosted_minutes": 0.0,
                                     "fleet_by_label": collections.Counter(), "queue": []})
            w["runs"] += 1
            if run["status"] != "completed":
                w["runs_unfinished"] += 1
            w["conclusions"][run["conclusion"] or run["status"] or "unknown"] += 1
            if ev == "push" and (run["head_branch"] == branch or not branch):
                d = dup[(run["name"], run["path"])]
                d["push_runs"] += 1
                d["same_sha_merge_group_success"] += (run["path"], run["head_sha"]) in mg_success
            landed = run["head_sha"] in push_shas
            nl = None
            if ev == "merge_group":
                nl = never[(run["name"], run["path"])]
                nl["runs"] += 1
                if run["status"] != "completed":
                    nl["in_flight"] += 1
                    nl = None
                else:
                    nl["runs_not_landed"] += not landed
            if jobs is None:
                w["runs_unread"] += 1
                continue
            for j in jobs:
                mins, q = job_cost(j)
                cls, label = runner_class(j.get("labels"))
                if cls == "fleet":
                    w["fleet_minutes"] += mins
                    w["fleet_by_label"][label] += mins
                else:
                    w["hosted_minutes"] += mins
                if q is not None:
                    w["queue"].append(q)
                bj = by_job.setdefault((run["name"], run["path"], ev, j["name"], cls, label), {
                    "workflow": run["name"], "path": run["path"], "event": ev, "job": j["name"], "class": cls,
                    "label": label, "jobs": 0, "ran": 0, "minutes": 0.0, "queue_minutes": 0.0,
                    "minutes_not_landed": 0.0})
                bj["jobs"] += 1
                bj["ran"] += q is not None
                bj["minutes"] += mins
                bj["queue_minutes"] += q or 0.0
                if nl is not None and not landed:
                    bj["minutes_not_landed"] += mins
                if nl is not None and cls == "fleet":
                    nl["fleet_minutes"] += mins
                    if not landed:
                        nl["fleet_minutes_not_landed"] += mins
                if cls == "fleet" and q is not None and mins < SHORT_JOB_MINUTES:
                    s = short.setdefault(label, {"label": label, "fleet_jobs": 0, "short_jobs": 0, "queue_minutes": 0.0,
                                                 "queue_minutes_all_jobs": 0.0})
                    s["short_jobs"] += 1
                    s["queue_minutes"] += q
                if cls == "fleet" and q is not None:
                    s = short.setdefault(label, {"label": label, "fleet_jobs": 0, "short_jobs": 0, "queue_minutes": 0.0,
                                                 "queue_minutes_all_jobs": 0.0})
                    s["fleet_jobs"] += 1
                    s["queue_minutes_all_jobs"] += q
    workflows = []
    for w in sorted(rows.values(), key=lambda r: -(r["fleet_minutes"] + r["hosted_minutes"])):
        workflows.append({
            "workflow": w["workflow"], "path": w["path"], "event": w["event"], "runs": w["runs"],
            "conclusions": dict(w["conclusions"]), "runs_unread": w["runs_unread"], "runs_unfinished": w["runs_unfinished"],
            "partial": bool(w["runs_unread"] or w["runs_unfinished"]),
            "fleet_minutes": round(w["fleet_minutes"], 1), "hosted_minutes": round(w["hosted_minutes"], 1),
            "fleet_by_label": {k: round(v, 1) for k, v in w["fleet_by_label"].items()},
            "queue_minutes": stats(w["queue"])})
    unfinished = sum(w["runs_unfinished"] for w in rows.values())
    reasons = list(data["partial"])
    if unfinished:
        reasons.append(f"{unfinished} runs had not finished; their unfinished jobs count as zero minutes")
    jobs_out = sorted(by_job.values(), key=lambda r: -r["minutes"])
    for j in jobs_out:
        for k in ("minutes", "queue_minutes", "minutes_not_landed"):
            j[k] = round(j[k], 1)
    report = {
        "schema": SCHEMA, "repo": repo, "since": since.isoformat(), "until": until.isoformat(),
        "events": list(events), "branch": branch,
        "partial": bool(reasons), "partial_reasons": reasons,
        "landing_horizon_days": LAND_HORIZON_DAYS,
        "reads": data["reads"], "read_errors": data["errors"],
        "workflows": workflows, "jobs": jobs_out,
        "duplicates": [{"workflow": k[0], "path": k[1], **v,
                        "share": round(v["same_sha_merge_group_success"] / v["push_runs"], 3) if v["push_runs"] else None}
                       for k, v in sorted(dup.items(), key=lambda kv: -kv[1]["push_runs"])],
        "short_job_wait": [{"label": s["label"], "fleet_jobs": s["fleet_jobs"], "short_jobs": s["short_jobs"],
                            "short_queue_hours": round(s["queue_minutes"] / 60, 1),
                            "all_queue_hours": round(s["queue_minutes_all_jobs"] / 60, 1)}
                           for s in sorted(short.values(), key=lambda s: -s["queue_minutes"])],
        "never_landed": [{"workflow": k[0], "path": k[1], "runs": v["runs"], "in_flight": v["in_flight"],
                          "runs_not_landed": v["runs_not_landed"], "fleet_minutes": round(v["fleet_minutes"], 1),
                          "fleet_minutes_not_landed": round(v["fleet_minutes_not_landed"], 1)}
                         for k, v in sorted(never.items(), key=lambda kv: -kv[1]["fleet_minutes"])],
    }
    return report


# ------------------------------------------------------------------ output
def render_md(rep):
    out = [f"# Workflow cost: {rep['repo']}, {rep['since']} to {rep['until']}", ""]
    if rep["partial"]:
        out += ["**PARTIAL.** Nothing below is a complete count:"] + [f"- {r}" for r in rep["partial_reasons"]] + [""]
    out += [f"Reads: {rep['reads']} ({rep['read_errors']} errors). Events: {', '.join(rep['events'])}. "
            f"Push runs counted on `{rep['branch']}`.", "",
            "## Per workflow and event", "",
            "| Workflow | Event | Runs | Conclusions | Fleet min | Hosted min | Queue median | Queue p90 | Queue sum (min) |",
            "|---|---|---|---|---|---|---|---|---|"]
    for w in rep["workflows"]:
        q = w["queue_minutes"]
        flag = " (partial)" if w["partial"] else ""
        out.append(f"| {w['workflow']}{flag} | {w['event']} | {w['runs']} | "
                   f"{', '.join(f'{v} {k}' for k, v in sorted(w['conclusions'].items()))} | {w['fleet_minutes']:,.0f} | "
                   f"{w['hosted_minutes']:,.0f} | {q['median']} | {q['p90']} | {q['sum']:,.0f} |")
    out += ["", "## Push runs that duplicate a merge-group run of the same SHA", "",
            "| Workflow | Push runs | Same-SHA merge-group success | Share |", "|---|---|---|---|"]
    for d in rep["duplicates"]:
        out.append(f"| {d['workflow']} | {d['push_runs']} | {d['same_sha_merge_group_success']} | "
                   f"{'' if d['share'] is None else format(d['share'], '.1%')} |")
    out += ["", f"## Fleet jobs under {SHORT_JOB_MINUTES:g} minutes, by label", "",
            "| Label | Fleet jobs | Short jobs | Short-job queue hours | All fleet queue hours |", "|---|---|---|---|---|"]
    for s in rep["short_job_wait"]:
        out.append(f"| {s['label']} | {s['fleet_jobs']} | {s['short_jobs']} | {s['short_queue_hours']} | {s['all_queue_hours']} |")
    out += ["", f"## Merge-group runs whose SHA did not reach the branch within {rep['landing_horizon_days']} days of the window", "",
            "| Workflow | Runs | In flight | Not landed | Fleet min | Fleet min not landed |", "|---|---|---|---|---|---|"]
    for n in rep["never_landed"]:
        out.append(f"| {n['workflow']} | {n['runs']} | {n['in_flight']} | {n['runs_not_landed']} | "
                   f"{n['fleet_minutes']:,.0f} | {n['fleet_minutes_not_landed']:,.0f} |")
    out += ["", "## Heaviest jobs (fleet or hosted)", "",
            "| Workflow | Event | Job | Class | Label | Jobs | Minutes | Queue min | Minutes not landed |",
            "|---|---|---|---|---|---|---|---|---|"]
    for j in rep["jobs"][:25]:
        out.append(f"| {j['workflow']} | {j['event']} | {j['job'][:50]} | {j['class']} | {j['label']} | {j['jobs']} | "
                   f"{j['minutes']:,.0f} | {j['queue_minutes']:,.0f} | {j['minutes_not_landed']:,.0f} |")
    return "\n".join(out) + "\n"


def main(argv=None, now=None, fetch=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--repo", required=True)
    ap.add_argument("--since", required=True, help="first UTC day, YYYY-MM-DD")
    ap.add_argument("--until", required=True, help="last UTC day, inclusive")
    ap.add_argument("--events", default=",".join(DEFAULT_EVENTS))
    ap.add_argument("--workflow", action="append", default=[], help="only read jobs for this workflow name or path (repeatable)")
    ap.add_argument("--branch", default="main", help="the branch whose push runs are counted")
    ap.add_argument("--cache", default=os.path.join(os.path.expanduser("~"), ".cache", "shipyard-workflow-cost"))
    ap.add_argument("--read-cap", type=int, default=DEFAULT_READ_CAP)
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--quiet", action="store_true", help="do not print the report")
    ap.add_argument("--json-out")
    ap.add_argument("--md-out")
    a = ap.parse_args(argv)
    since, until = dt.date.fromisoformat(a.since), dt.date.fromisoformat(a.until)
    events = tuple(e for e in a.events.split(",") if e)
    if until < since or not set(events) <= set(DEFAULT_EVENTS):
        ap.error("bad window or events")
    today = (now or dt.datetime.now(UTC)).date()
    rd = CostReader(a.repo, a.cache, a.read_cap, fetch=fetch)
    data = collect(rd, since, until, events, a.workflow, a.branch, today, a.workers)
    rep = compute(data, a.repo, since, until, events, a.branch)
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

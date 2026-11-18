"""Synthetic control for the scorecard: a known injected case, run through the real pipeline on every report.

`CASES` is a table of fifteen invented PRs, each built to have one known outcome. `raw_world` turns the table into
the GitHub answers the scorecard reads (PR list, timelines, commit lists, base commits, check runs, workflow runs,
jobs, the base's config); `expected` derives every number from the table alone, with plain arithmetic and no
call into `pr_metrics`; `run` feeds the raw world to `pr_metrics.build` and compares. A report prints
CONTROL PASS or CONTROL FAIL with the deltas, and withholds its outcome numbers when the control fails, because
an instrument that cannot reproduce numbers it was built to produce says nothing about live data.

The control reads nothing from GitHub and writes only to a throwaway cache directory.
"""
from __future__ import annotations

import base64
import datetime as dt
import math
import re
import statistics
import tempfile

UTC = dt.timezone.utc
REPO = "control/synthetic"
OWNER = "danielraffel"
APP = "shipyard-local[bot]"
DAY = "2026-11-10"
NOW = dt.datetime(2026, 11, 30, 12, 0, tzinfo=UTC)  # past every horizon and window: correctness is final
START = dt.datetime(2026, 11, 1, tzinfo=UTC)
CONFIG = '[auto_merge]\nreviewer_signers = ["rev"]\nreviewer_stamp_sessions = ["fleet-pass-reviewer-*"]\n'


def _t(hm, day=10):
    return f"2026-11-{day:02d}T{hm}:00Z"


def _plus(minutes):
    h, m = divmod(600 + minutes, 60)  # PRs become ready at 10:00
    return _t(f"{h:02d}:{m:02d}")


# One row per invented PR. `north` is the outcome the scorecard must give it; every other field is a
# fact of how the PR was built. `delay` is minutes from ready to merge (None: never merged).
CASES = [
    {"n": 1, "name": "auto_a", "delay": 20, "north": "autonomous"},
    {"n": 2, "name": "auto_b", "delay": 30, "north": "autonomous"},
    {"n": 3, "name": "reverted", "delay": 25, "north": "autonomous", "reverted": True},
    {"n": 4, "name": "base_red", "delay": 35, "north": "autonomous", "base_red": True},
    {"n": 5, "name": "escaped", "delay": 75, "north": "autonomous", "escaped_by": 6},
    {"n": 6, "name": "later_fix", "delay": 120, "north": "autonomous", "title": "fix regression from #5"},
    {"n": 7, "name": "held_twice", "delay": 150, "north": "autonomous", "holds": 2, "re_holds": 1,
     "pushes": {"author": 1, "repair": 1, "merge_main": 1}, "slow": True, "cause_min": {"hold": None}},
    {"n": 8, "name": "queued_twice", "delay": 85, "north": "autonomous", "enqueues": 2, "ejections": ["failed_checks"], "enq_to_merge": 45, "slow": True, "cause_min": {"queue": 55}},
    {"n": 9, "name": "compute", "delay": 95, "north": "autonomous", "runs": 4, "repeat_runs": 2, "macos_min": 10, "slow": True, "cause_min": {"red": 83, "runner": 6},  # red: run 101 failed at 10:12 and no push followed, so 83 min to the 11:35 merge; three runs that can precede the merge wait 2 min each; the push on the merge-group sha runs after it
     "workflows": {"ci": {"pull_request": 2, "merge_group": 1, "push": 1}}},
    {"n": 10, "name": "owner_label_a", "delay": 100, "north": "owner_action", "owner_actions": 1},
    {"n": 11, "name": "owner_label_b", "delay": 110, "north": "owner_action", "owner_actions": 1},
    {"n": 12, "name": "steward_merge", "delay": 45, "north": "autonomous", "enqueues": 1, "steward": True, "enq_to_merge": 25},
    {"n": 13, "name": "unbound_owner_merge", "delay": 55, "north": "unknown", "enqueues": 1, "unverified": True, "enq_to_merge": 35},
    {"n": 14, "name": "owner_opened_unsigned", "delay": 65, "north": "unknown", "opener": OWNER, "signal": None},
    {"n": 15, "name": "never_merged", "delay": None, "north": "not_merged"},
]


def _sha(n, tag="a"):
    return f"{tag}{n:039x}"


def raw_world():
    """The GitHub answers for CASES, as a `path -> JSON` function."""
    prs, timelines, commits, runs, jobs, checks, base_commits = [], {}, {}, [], {}, {}, []
    for c in CASES:
        n = c["n"]
        merged = _plus(c["delay"]) if c["delay"] is not None else None
        head = _sha(n, "b")
        merge_sha = _sha(n, "c")
        opener = c.get("opener", APP)
        pr = {"number": n, "created_at": _t("10:00"), "merged_at": merged, "updated_at": merged or _t("10:00"),
              "state": "closed" if merged else "open", "merge_commit_sha": merge_sha if merged else None,
              "base": {"ref": "main"}, "head": {"sha": head}, "user": {"login": opener},
              "title": c.get("title", f"case {n} {c['name']}"), "body": "", "labels": [], "draft": False}
        prs.append(pr)
        tl, cm = [], [{"sha": head, "author": {"login": OWNER}, "committer": {"login": OWNER},
                       "commit": {"message": "work", "committer": {"date": _t("09:00")}}}]  # owner identity, no trailer
        if c.get("steward") or c.get("unverified") or c.get("enqueues"):
            if c.get("steward"):
                tl.append({"event": "committed", "sha": head})  # the timeline binds the enqueue to the merged SHA
            if c.get("name") == "queued_twice":
                tl += [{"event": "added_to_merge_queue", "actor": {"login": APP}, "created_at": _t("10:20")},
                       {"event": "removed_from_merge_queue", "actor": {"login": APP}, "created_at": _t("10:30"), "reason": "failed_checks"},
                       {"event": "added_to_merge_queue", "actor": {"login": APP}, "created_at": _t("10:40")}]
            else:
                tl.append({"event": "added_to_merge_queue", "actor": {"login": APP}, "created_at": _t("10:20")})
        if c.get("steward") or c.get("unverified"):
            tl.append({"event": "merged", "actor": {"login": OWNER}, "created_at": merged})
        elif merged:
            tl.append({"event": "merged", "actor": {"login": APP}, "created_at": merged})
        if merged:  # GitHub records a removal from the queue when a PR merges; it is not an ejection
            tl.append({"event": "removed_from_merge_queue", "actor": {"login": APP}, "created_at": merged})
        if c.get("owner_actions"):
            tl.append({"event": "labeled", "actor": {"login": OWNER}, "created_at": _t("10:05")})
        if c.get("holds"):
            tl += [{"event": "commented", "user": {"login": "reviewer[bot]"}, "created_at": _t("10:20"), "body": "HOLD: fix this"},
                   {"event": "commented", "user": {"login": "reviewer[bot]"}, "created_at": _t("10:40"), "body": "HOLD: still broken"},
                   {"event": "committed", "sha": _sha(n, "e"), "committer": {"date": _t("10:30")}}]  # GitHub's `committed` event has no observation time: the repair push time is unknown
            cm = [{"sha": sha, "author": {"login": APP}, "committer": {"login": APP}, "commit": {"message": m, "committer": {"date": d}}}
                  for sha, m, d in ((head, "work", _t("10:10")), (_sha(n, "e"), "fix", _t("10:30")),
                                    (_sha(n, "f"), "Merge branch 'main' into x", _t("10:50")))]
        timelines[n], commits[n] = tl, cm
        if c.get("reverted"):
            base_commits.append({"sha": _sha(n, "9"), "commit": {"message": f'Revert "case {n}" (#{n})', "committer": {"date": _t("20:00")}}})
        if c.get("base_red"):
            checks[merge_sha] = [{"name": "Test", "status": "completed", "conclusion": "failure", "started_at": merged}]
        if c.get("runs"):
            g = "9" * 40
            for rid, ev, sha, br, lab, mins in ((101, "pull_request", head, "feat", ["self-hosted", "macOS"], 10), (102, "pull_request", head, "feat", ["ubuntu"], 5),
                                                (103, "merge_group", g, f"gh-readonly-queue/main/pr-{n}-x", ["self-hosted"], 20), (104, "push", g, "main", ["ubuntu"], 1)):
                runs.append({"id": rid, "name": "ci", "head_sha": sha, "event": ev, "status": "completed", "head_branch": br,
                             "conclusion": "failure" if rid == 101 else "success", "updated_at": _t("10:12")})  # run 101 failed, at 10:12
                jobs[rid] = [{"labels": lab, "created_at": _t("09:58"), "started_at": _t("10:00"), "completed_at": _plus(mins), "conclusion": "success"}]
    ok = [{"name": "Test", "status": "completed", "conclusion": "success"}]

    def get(path):
        base = f"repos/{REPO}/"
        if not path.startswith(base):
            raise KeyError(path)
        p = path[len(base):]
        if p.startswith("pulls?state=all"):
            return prs
        m = re.match(r"issues/(\d+)/timeline", p)
        if m:
            return timelines[int(m.group(1))]
        m = re.match(r"pulls/(\d+)/commits", p)
        if m:
            return commits[int(m.group(1))]
        if re.match(r"pulls/\d+$", p):
            return {"additions": 300, "deletions": 100}
        if p.startswith("commits?sha=main"):
            return base_commits
        if p.startswith("actions/runs?created"):
            return {"workflow_runs": runs}
        m = re.match(r"actions/runs/(\d+)/jobs", p)
        if m:
            return {"jobs": jobs[int(m.group(1))]}
        m = re.match(r"commits/([^/]+)/check-runs", p)
        if m:
            return {"check_runs": checks.get(m.group(1), ok)}
        if p.startswith("contents/.shipyard/config.toml"):
            return {"content": base64.b64encode(CONFIG.encode()).decode()}
        if p.startswith("rules/branches/main"):
            return [{"type": "required_status_checks", "parameters": {"required_status_checks": [{"context": "Test"}]}}]
        raise KeyError(path)
    return get


def expected():
    """Every number the control checks, from CASES alone. No scorecard code is called."""
    n = len(CASES)
    merged = [c for c in CASES if c["delay"] is not None]
    count = lambda k: sum(1 for c in CASES if c["north"] == k)
    hand = sorted(c["delay"] for c in merged)
    q2m = [c["enq_to_merge"] for c in CASES if "enq_to_merge" in c]
    slow = [c for c in merged if c.get("slow")]
    owner_actions = sum(c.get("owner_actions", 0) for c in CASES)
    ejections = sum(len(c.get("ejections", [])) for c in CASES)
    bad = sum(1 for c in merged if c.get("reverted") or c.get("base_red"))
    return {
        "ready_prs": n, "merged": len(merged),
        "north_star.autonomous": count("autonomous"), "north_star.owner_action": count("owner_action"),
        "north_star.unknown": count("unknown"), "north_star.not_merged_in_48h": count("not_merged"),
        "north_star.rate_pct": round(100.0 * count("autonomous") / n, 1),
        "handoff_min.median": round(statistics.median(hand), 1),
        "handoff_min.p90": round(hand[max(0, math.ceil(0.9 * len(hand)) - 1)], 1),
        "rounds.holds_per_ready_pr": round(sum(c.get("holds", 0) for c in CASES) / n, 2),
        "rounds.prs_with_hold": sum(1 for c in CASES if c.get("holds")),
        "rounds.prs_with_re_hold": sum(1 for c in CASES if c.get("re_holds")),
        "pushes_total.author": sum(c.get("pushes", {}).get("author", 0) for c in CASES),
        "pushes_total.repair": sum(c.get("pushes", {}).get("repair", 0) for c in CASES),
        "pushes_total.merge_main": sum(c.get("pushes", {}).get("merge_main", 0) for c in CASES),
        "merge_queue.attempts_per_ready_pr": round(sum(c.get("enqueues", 0) for c in CASES) / n, 2),
        "merge_queue.ejections_failed_checks": ejections,
        "interventions.owner_actions_per_pr": round(owner_actions / n, 2),
        "interventions.prs_with_owner_action": sum(1 for c in CASES if c.get("owner_actions")),
        "compute_per_merged_pr.runs_total": sum(c.get("runs", 0) for c in CASES),
        "compute_per_merged_pr.repeat_runs_total": sum(c.get("repeat_runs", 0) for c in CASES),
        "compute_per_merged_pr.prs_read": len(merged),
        "compute_per_merged_pr.macos_min_mean": round(sum(c.get("macos_min", 0) for c in merged) / len(merged), 1),
        "compute_per_merged_pr.macos_min_median": round(statistics.median(c.get("macos_min", 0) for c in merged), 1),
        "merge_queue.enqueue_to_merge_min.n": len(q2m),
        "merge_queue.enqueue_to_merge_min.median": round(statistics.median(q2m), 1),
        **{f"compute_per_merged_pr.by_workflow.{wf}.{lv}": round(sum(c.get("workflows", {}).get(wf, {}).get(lv, 0) for c in merged) / len(merged), 2)
           for wf in {w for c in merged for w in c.get("workflows", {})} for lv in ("pull_request", "merge_group", "push")},
        "paths.slow.n": len(slow), "paths.clean.n": len(merged) - len(slow), "paths.unknown": 0,
        "paths.slow_share_pct": round(100.0 * len(slow) / len(merged), 1),
        "paths.clean.median_min": round(statistics.median(c["delay"] for c in merged if not c.get("slow")), 1),
        "paths.slow.median_min": round(statistics.median(c["delay"] for c in slow), 1),
        "paths.slow.p90_min": sorted(c["delay"] for c in slow)[max(0, math.ceil(0.9 * len(slow)) - 1)],
        # A cause aggregate is valid only when every slow PR has a measured value. Missing
        # values stay unknown instead of being treated as zero-minute contributions.
        **{f"paths.cause_min_per_merge.{k}": (round(sum(c.get("cause_min", {}).get(k, 0) for c in slow) / len(merged), 2)
                                              if slow and all(c.get("cause_min", {}).get(k, 0) is not None for c in slow) else None)
           for k in ("hold", "red", "queue", "runner")},
        "correctness.bad": bad, "correctness.good": len(merged) - bad,
        "correctness.reverted": sum(1 for c in merged if c.get("reverted")),
        "correctness.base_red": sum(1 for c in merged if c.get("base_red")),
        "correctness.escaped": sum(1 for c in merged if c.get("escaped_by")),
        "attribution.steward_merges_recognised": sum(1 for c in CASES if c.get("steward")),
        "attribution.owner_merges_unverified": sum(1 for c in CASES if c.get("unverified")),
        "attribution.by_primary.app_opener": sum(1 for c in CASES if c.get("opener", APP) == APP),
        "attribution.by_primary.none": sum(1 for c in CASES if c.get("opener", APP) != APP),
    }


def _dig(d, dotted):
    for k in dotted.split("."):
        if not isinstance(d, dict) or k not in d:
            return None
        d = d[k]
    return d


def run(pr_metrics, tolerance=0.005):
    """Run the real scorecard over the injected world. Returns {"status", "checks", "failed", "max_abs_delta"}.
    Each check is {"name", "expected", "got", "delta"}; a number that is missing counts as a failure."""
    want = expected()
    got_summary = None
    error = None
    try:
        with tempfile.TemporaryDirectory() as cache:
            out = pr_metrics.build([REPO], cache, 10_000, OWNER, START, NOW, {}, {}, None, raw_world(), controls={}, control=False)
        got_summary = out["repos"][REPO]["per_day"][DAY]
    except Exception as e:  # an instrument that crashes has failed its control
        error = f"{type(e).__name__}: {e}"
    checks, failed = [], []
    for name, exp in want.items():
        got = None if got_summary is None else _dig(got_summary, name)
        if name == "merge_queue.ejections_failed_checks" and got_summary is not None:  # a cause missing from the map is zero ejections
            got = (got_summary["merge_queue"].get("ejections_by_cause") or {}).get("failed_checks", 0)
        if exp is None:
            delta = 0.0 if got is None else None
            ok = got is None
        else:
            delta = None if got is None else round(float(got) - float(exp), 4)
            ok = delta is not None and abs(delta) <= tolerance
        checks.append({"name": name, "expected": exp, "got": got, "delta": delta})
        if not ok:
            failed.append(name)
    deltas = [abs(c["delta"]) for c in checks if c["delta"] is not None]
    return {"status": "fail" if failed or error else "pass", "checks": checks, "failed": failed, "error": error,
            "max_abs_delta": max(deltas) if deltas else None}


def line(result):
    """The one-line verdict a report prints, with the deltas of whatever missed."""
    n = len(result["checks"])
    if result["status"] == "pass":
        return f"CONTROL PASS: {n} of {n} injected-case numbers reproduced (max abs delta {result['max_abs_delta']})"
    bad = [c for c in result["checks"] if c["name"] in result["failed"]]
    why = "; ".join(f"{c['name']} expected {c['expected']} got {c['got']} (delta {c['delta']})" for c in bad) or result.get("error") or "no result"
    if result.get("error"):
        why = f"{result['error']}; " + why
    return f"CONTROL FAIL: {len(result['failed'])} of {n} injected-case numbers off: {why}"

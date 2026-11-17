import datetime as dt
import json
import os
import re
import base64
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import pr_metrics as pm  # noqa: E402
import synthetic_control as sc  # noqa: E402
import acceptance as acc  # noqa: E402
import loss_report as lr  # noqa: E402

UTC = dt.timezone.utc
REPO = "acme/repo"
OWNER = "danielraffel"
NOW = dt.datetime(2026, 11, 14, 12, 0, tzinfo=UTC)
START = dt.datetime(2026, 11, 1, tzinfo=UTC)


def T(day, hm):
    return f"2026-11-{day:02d}T{hm}:00Z"


def pr(n, created, merged, sha=None):
    return {"number": n, "created_at": created, "merged_at": merged, "updated_at": merged,
            "state": "closed", "merge_commit_sha": sha or f"m{n:040d}"[:40], "base": {"ref": "main"}, "head": {"sha": f"h{n:040d}"[:40]}}


def commit(sha, login="shipyard-local[bot]", msg="x", date=T(1, "00:00")):
    return {"sha": sha, "author": {"login": login}, "committer": {"login": login},
            "commit": {"message": msg, "committer": {"date": date}}}


class Fake:
    """GitHub stand-in: routes are (regex, value); a missing route is a read error."""

    def __init__(self, routes):
        self.routes, self.calls = routes, []

    def __call__(self, path):
        self.calls.append(path)
        for rx, val in self.routes:
            if re.search(rx, path):
                return val(path) if callable(val) else val
        raise pm.ReadError("no route " + path)


CONFIG = ('[auto_merge]\nreviewer_signers = ["rev"]\nreviewer_stamp_sessions = ["fleet-pass-reviewer-*"]\n')
HEAD = "a" * 40


def world(prs, timelines=None, commits=None, base_commits=None, runs=None, jobs=None, checks=None, required=("Test",), default_checks=True):
    timelines, commits, jobs, checks = timelines or {}, commits or {}, jobs or {}, checks or {}
    routes = [(r"pulls\?state=all", prs)]
    for n in {p["number"] for p in prs}:
        routes.append((rf"issues/{n}/timeline", timelines.get(n, [])))
        routes.append((rf"pulls/{n}/commits", commits.get(n, [commit(f"c{n}")])))
    for n in {p["number"] for p in prs}:
        routes.append((rf"pulls/{n}$", {"additions": 300, "deletions": 100}))
    routes.append((r"/commits\?sha=main", base_commits or []))
    routes.append((r"actions/runs\?created", {"workflow_runs": runs or []}))
    for rid, js in jobs.items():
        routes.append((rf"actions/runs/{rid}/jobs", {"jobs": js}))
    for sha, cr in checks.items():
        routes.append((rf"commits/{sha}/check-runs", (lambda path, cr=cr: {"check_runs": cr(path)}) if callable(cr) else {"check_runs": cr}))
    if default_checks:
        routes.append((r"commits/[^/]+/check-runs", {"check_runs": [{"name": "Test", "status": "completed", "conclusion": "success"}]}))
    routes.append((r"contents/\.shipyard/config\.toml", {"content": base64.b64encode(CONFIG.encode()).decode()}))
    routes.append((r"rules/branches/main", [{"type": "required_status_checks",
                                              "parameters": {"required_status_checks": [{"context": c} for c in required]}}]))
    return Fake(routes)


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.cache = self.tmp.name

    def tearDown(self):
        self.tmp.cleanup()

    def build(self, gh, cap=200, ticks=None, now=NOW, baseline=None):
        return pm.build([REPO], self.cache, cap, OWNER, START, now, ticks or {}, {}, baseline, gh)

    def rows(self, out):
        return {r["pr"]: r for r in out["repos"][REPO]["prs"]}


class SharedCap(Base):
    def test_total_reads_never_exceed_cap_when_cap_below_repo_count(self):
        repos = [f"o/r{i}" for i in range(5)]
        gh = world([pr(1, T(10, "10:00"), T(10, "11:00"))])
        out = pm.build(repos, self.cache, 1, OWNER, START, NOW, {}, {}, None, gh)
        self.assertLessEqual(out["reads"], 1)
        self.assertLessEqual(len(gh.calls), 1)
        self.assertTrue(out["truncated"])
        self.assertEqual(out["coverage"]["status"], "partial")

    def test_zero_cap_issues_no_reads(self):
        gh = world([pr(1, T(10, "10:00"), T(10, "11:00"))])
        out = pm.build([REPO, "o/other"], self.cache, 0, OWNER, START, NOW, {}, {}, None, gh)
        self.assertEqual(len(gh.calls), 0)
        self.assertTrue(out["truncated"])


class Autonomy(Base):
    def test_owner_label_is_an_owner_action_and_bot_events_are_not(self):
        tl = {1: [{"event": "labeled", "actor": {"login": "shipyard-local[bot]"}, "created_at": T(10, "10:30")}],
              2: [{"event": "labeled", "actor": {"login": OWNER}, "created_at": T(10, "10:30")}]}
        out = self.build(world([pr(1, T(10, "10:00"), T(10, "11:00")), pr(2, T(10, "10:00"), T(10, "11:00"))], tl))
        r = self.rows(out)
        self.assertEqual((r[1]["north"], r[2]["north"]), ("autonomous", "owner_action"))
        a = out["repos"][REPO]["per_day"]["2026-11-10"]["north_star"]
        self.assertEqual((a["autonomous"], a["owner_action"], a["unknown"], a["rate_pct"]), (1, 1, 0, 50.0))

    def test_owner_commit_without_agent_trailer_is_unknown_with_one_is_autonomous(self):
        cm = {1: [commit("c1", OWNER, "fix it")],
              2: [commit("c2", OWNER, "fix it\n\nCo-Authored-By: Claude Sonnet <noreply@anthropic.com>")]}
        out = self.build(world([pr(1, T(10, "10:00"), T(10, "11:00")), pr(2, T(10, "10:00"), T(10, "11:00"))], commits=cm))
        r = self.rows(out)
        self.assertEqual((r[1]["north"], r[2]["north"]), ("unknown", "autonomous"))
        a = out["repos"][REPO]["per_day"]["2026-11-10"]["north_star"]
        self.assertEqual((a["unknown"], a["rate_pct"], a["rate_range_pct"]), (1, 50.0, [50.0, 100.0]))  # unknown PRs stay in the denominator

    def test_version_bump_trailer_marks_agent_work_and_github_only_ignores_commits(self):
        cm = {1: [commit("c1", OWNER, "chore\n\nVersion-Bump: cli=skip reason=x")], 2: [commit("c2", OWNER, "by hand")]}
        out = self.build(world([pr(1, T(10, "10:00"), T(10, "11:00")), pr(2, T(10, "10:00"), T(10, "11:00"))], commits=cm))
        r = self.rows(out)
        self.assertEqual((r[1]["north"], r[2]["north"], r[2]["north_gh"]), ("autonomous", "unknown", "autonomous"))
        a = out["repos"][REPO]["per_day"]["2026-11-10"]["north_star"]
        self.assertEqual((a["unknown_share_pct"], a["github_only"]["unknown"], a["github_only"]["rate_pct"], a["unknown_over_20pct"]), (50.0, 0, 100.0, True))

    def test_a_definite_owner_action_beats_an_ambiguous_commit(self):
        tl = {1: [{"event": "commented", "user": {"login": OWNER}, "created_at": T(10, "10:30")}]}
        out = self.build(world([pr(1, T(10, "10:00"), T(10, "11:00"))], tl, {1: [commit("c1", OWNER)]}))
        self.assertEqual(self.rows(out)[1]["north"], "owner_action")

    def test_unreadable_timeline_leaves_the_pr_unread_not_autonomous(self):
        gh = world([pr(1, T(10, "10:00"), T(10, "11:00"))])
        gh.routes = [x for x in gh.routes if "timeline" not in x[0]]
        out = self.build(gh)
        self.assertEqual(out["repos"][REPO]["unread_prs"], 1)
        self.assertEqual(out["repos"][REPO]["prs"], [])
        self.assertEqual(out["coverage"]["status"], "partial")


class Handoff(Base):
    def test_opened_non_draft_counts_from_opening(self):
        out = self.build(world([pr(1, T(10, "10:00"), T(10, "11:30"))]))
        self.assertEqual(self.rows(out)[1]["handoff_min"], 90.0)

    def test_opened_as_draft_counts_from_ready_for_review(self):
        tl = {1: [{"event": "ready_for_review", "actor": {"login": "x[bot]"}, "created_at": T(10, "12:00")}]}
        out = self.build(world([pr(1, T(10, "10:00"), T(10, "13:00"))], tl))
        self.assertEqual(self.rows(out)[1]["handoff_min"], 60.0)

    def test_converted_to_draft_later_still_counts_from_opening(self):
        tl = {1: [{"event": "converted_to_draft", "actor": {"login": "x[bot]"}, "created_at": T(10, "10:30")},
                  {"event": "ready_for_review", "actor": {"login": "x[bot]"}, "created_at": T(10, "12:00")}]}
        out = self.build(world([pr(1, T(10, "10:00"), T(10, "13:00"))], tl))
        self.assertEqual(self.rows(out)[1]["handoff_min"], 180.0)

    def test_median_and_p90(self):
        prs = [pr(i, T(10, "10:00"), T(10, f"{10 + i:02d}:00")) for i in range(1, 11)]  # 60..600 min
        h = self.build(world(prs))["repos"][REPO]["per_day"]["2026-11-10"]["handoff_min"]
        self.assertEqual((h["median"], h["p90"], h["n"]), (330.0, 540.0, 10))


class Compute(Base):
    def setUp(self):
        super().setUp()
        sha1 = f"h{1:040d}"[:40]
        self.runs = [
            {"id": 11, "head_sha": sha1, "event": "pull_request", "status": "completed", "head_branch": "feat"},
            {"id": 12, "head_sha": "zzz", "event": "merge_group", "status": "completed",
             "head_branch": "gh-readonly-queue/main/pr-1-abc"},
            {"id": 13, "head_sha": "zzz", "event": "merge_group", "status": "completed",
             "head_branch": "gh-readonly-queue/main/pr-10-abc"},  # another PR: pr-10 is not pr-1
            {"id": 14, "head_sha": "other", "event": "push", "status": "completed", "head_branch": "main"},
        ]
        job = lambda lab, a, b, concl="success": {"labels": lab, "started_at": T(10, a), "completed_at": T(10, b), "conclusion": concl}
        self.jobs = {11: [job(["self-hosted", "macOS"], "10:00", "10:20"), job(["ubuntu-latest"], "10:00", "10:05")],
                     12: [job(["self-hosted"], "10:40", "10:50"), job(["x"], "10:40", "10:45", "skipped")],
                     13: [job(["ubuntu-latest"], "10:00", "11:00")], 14: [job(["ubuntu-latest"], "10:00", "11:00")]}

    def test_minutes_over_head_and_merge_group_runs_split_by_runner(self):
        out = self.build(world([pr(1, T(10, "09:00"), T(10, "11:00"))], runs=self.runs, jobs=self.jobs))
        c = self.rows(out)[1]["compute"]
        self.assertEqual((c["self_hosted_min"], c["hosted_min"], c["runs"], c["complete"]), (30.0, 5.0, 2, True))

    def test_each_run_and_each_pr_is_read_once_ever(self):
        gh = world([pr(1, T(10, "09:00"), T(10, "11:00"))], runs=self.runs, jobs=self.jobs)
        self.build(gh)
        first = [c for c in gh.calls if "/jobs" in c or "/timeline" in c or "pulls/1/commits" in c]
        gh2 = world([pr(1, T(10, "09:00"), T(10, "11:00"))], runs=self.runs, jobs=self.jobs)
        out = self.build(gh2)
        again = [c for c in gh2.calls if "/jobs" in c or "/timeline" in c or "pulls/1/commits" in c or "actions/runs?created" in c]
        self.assertEqual(len(first), 4)
        self.assertEqual(again, [])
        self.assertEqual(self.rows(out)[1]["compute"]["self_hosted_min"], 30.0)

    def test_a_current_compute_entry_is_not_trusted_while_the_head_reads_are_incomplete(self):
        d = os.path.join(self.cache, REPO.replace("/", "__"), "compute")
        os.makedirs(d)
        with open(os.path.join(d, "1.json"), "w") as fh:
            json.dump({"v": pm.COMPUTE_VERSION, "self_hosted_min": 1.0, "hosted_min": 1.0, "runs": 1, "repeat_runs": 0, "group_runs": 0, "complete": True}, fh)
        gh = world([pr(1, T(10, "09:00"), T(10, "11:00"))], runs=self.runs, jobs=self.jobs)
        gh.routes.insert(0, (r"pulls/1/commits", lambda path: [commit(f"p{i}", "x", "w") for i in range(100)]))  # 3 full pages: incomplete
        c = self.rows(self.build(gh))[1]["compute"]
        self.assertFalse((c or {}).get("complete"))
        self.assertNotEqual((c or {}).get("self_hosted_min"), 1.0)

    def test_an_unreadable_jobs_list_is_unread_not_zero(self):
        gh = world([pr(1, T(10, "09:00"), T(10, "11:00"))], runs=self.runs, jobs={})
        out = self.build(gh)
        self.assertIsNone(self.rows(out)[1]["compute"])
        self.assertEqual(out["repos"][REPO]["per_day"]["2026-11-10"]["compute_per_merged_pr"]["prs_read"], 0)


class ProofsAndQueue(Base):
    def test_a_busy_run_range_is_split_until_each_part_fits_and_is_complete(self):
        seen = []
        full = dt.datetime(2026, 11, 14, tzinfo=UTC), dt.datetime(2026, 11, 14, 23, 59, 59, tzinfo=UTC)
        span = (full[1] - full[0]).total_seconds()

        def fetch(path):
            m = re.search(r"created=([^.]+)\.\.([^&]+)&per_page=100&page=(\d+)", path)
            a, b = (dt.datetime.strptime(x, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=UTC) for x in m.groups()[:2])
            total = int(2500 * ((b - a).total_seconds() + 1) / (span + 1))
            seen.append((a, b, int(m.group(3)), total))
            start = (int(m.group(3)) - 1) * 100
            n = max(0, min(100, total - start))
            return {"total_count": total, "workflow_runs": [{"id": int(a.timestamp()) * 1000 + start + i} for i in range(n)]}
        rd = pm.Reader(REPO, self.cache, 10_000, fetch)
        runs, complete = pm.runs_index(rd, *full)
        self.assertTrue(complete)
        self.assertTrue(all(x[3] <= pm.RUNS_SLICE_LIMIT for x in seen if x[2] == 1 and x[3] <= pm.RUNS_SLICE_LIMIT) and max(x[2] for x in seen) <= 10)
        self.assertGreater(len(runs), 2000)  # not cut at 1000
        self.assertEqual(len({r["id"] for r in runs}), len(runs))

    def test_a_range_that_cannot_be_split_further_and_overflows_is_incomplete(self):
        fetch = lambda path: {"total_count": 5000, "workflow_runs": [{"id": 1}] * 100}
        rd = pm.Reader(REPO, self.cache, 10_000, fetch)
        t0 = dt.datetime(2026, 11, 14, tzinfo=UTC)
        _, complete = pm.runs_index(rd, t0, t0 + dt.timedelta(seconds=1))
        self.assertFalse(complete)

    def test_an_old_job_cache_without_macos_minutes_is_read_again(self):
        rd = pm.Reader(REPO, self.cache, 100, lambda path: {"jobs": [{"labels": ["macOS-14", "self-hosted"], "started_at": T(10, "10:00"),
                                                                       "completed_at": T(10, "10:30"), "conclusion": "success"}]})
        rd.save("jobs", "7", {"self_hosted": 99.0, "hosted": 0.0})
        sh, ho, mac, wait, ok = pm.job_minutes(rd, {"id": 7, "status": "completed"})
        self.assertEqual((sh, ho, mac, ok), (30.0, 0.0, 30.0, True))

    def test_enqueue_to_merge_counts_from_the_last_enqueue_and_runs_by_workflow_and_level_are_kept(self):
        p = pr(1, T(10, "10:00"), T(10, "11:25"))
        tl = {1: [{"event": "added_to_merge_queue", "actor": {"login": "x"}, "created_at": T(10, "10:20")},
                  {"event": "removed_from_merge_queue", "actor": {"login": "x"}, "created_at": T(10, "10:30"), "reason": "failed_checks"},
                  {"event": "added_to_merge_queue", "actor": {"login": "x"}, "created_at": T(10, "10:40")}]}
        sha = f"h{1:040d}"[:40]
        runs = [{"id": 1, "name": "Proof", "head_sha": sha, "event": "pull_request", "status": "completed"},
                {"id": 2, "name": "Proof", "head_sha": "g", "event": "merge_group", "status": "completed", "head_branch": "gh-readonly-queue/main/pr-1-x"}]
        job = [{"labels": ["macOS"], "started_at": T(10, "10:00"), "completed_at": T(10, "10:12"), "conclusion": "success"}]
        other = pr(2, T(10, "10:00"), T(10, "11:00"))
        other["head"]["sha"] = "z" * 40
        out = self.build(world([p, other], tl, commits={2: [commit("c2")]}, runs=runs, jobs={1: job, 2: job}))
        r = self.rows(out)
        self.assertEqual((r[1]["enqueue_to_merge_min"], r[2]["enqueue_to_merge_min"]), (45.0, None))
        c = r[1]["compute"]
        self.assertEqual((c["macos_min"], c["by_workflow"]["Proof"]), (24.0, {"pull_request": 1, "merge_group": 1, "push": 0, "other": 0}))
        s = out["repos"][REPO]["per_day"]["2026-11-10"]
        self.assertEqual((s["merge_queue"]["enqueue_to_merge_min"]["n"], s["merge_queue"]["enqueue_to_merge_min"]["median"]), (1, 45.0))
        self.assertEqual(s["compute_per_merged_pr"]["by_workflow"]["Proof"], {"pull_request": 0.5, "merge_group": 0.5, "push": 0.0, "prs": 2})
        md = pm.markdown(out)
        self.assertIn("Proofs, macOS minutes and queue wait per merged PR", md)
        self.assertIn("| Proof | 0.5 / 0.5 / 0.0 |", md)


class Paths(unittest.TestCase):
    F = {"holds": 0, "ejections": [], "merged_at": T(10, "12:00"), "hold_times": [], "push_times": [], "enqueue_times": [], "dequeue_times": [],
         "timeline_complete": True, "commits_complete": True}

    def test_cause_minutes_are_none_when_the_timeline_or_the_commits_were_not_read_in_full(self):
        f = self.f(hold_times=[T(10, "10:00")], push_times=[T(10, "10:20")])
        self.assertEqual(pm.cause_minutes(f, self.comp())["hold"], 20.0)
        self.assertIsNone(pm.cause_minutes(dict(f, timeline_complete=False), self.comp()))
        self.assertIsNone(pm.cause_minutes(dict(f, commits_complete=False), self.comp()))

    def f(self, **kw):
        return dict(self.F, **kw)

    def comp(self, **kw):
        return dict({"complete": True, "red_events": [], "runner_wait_min": 0.0}, **kw)

    def test_a_pr_with_no_slow_cause_and_a_complete_compute_is_clean(self):
        self.assertEqual(pm.path_of(self.f(), self.comp()), ("clean", []))

    def test_each_slow_cause_makes_the_pr_slow_with_its_reason(self):
        self.assertEqual(pm.path_of(self.f(holds=1), self.comp()), ("slow", ["hold"]))
        self.assertEqual(pm.path_of(self.f(), self.comp(red_events=[T(10, "10:30")])), ("slow", ["red"]))
        self.assertEqual(pm.path_of(self.f(ejections=["failed_checks"]), self.comp()), ("slow", ["ejection"]))
        self.assertEqual(pm.path_of(self.f(dirty=True), self.comp()), ("slow", ["dirty"]))
        self.assertEqual(pm.path_of(self.f(holds=1, dirty=True), self.comp(red_events=[T(10, "10:30")])), ("slow", ["hold", "red", "dirty"]))

    def test_an_unread_compute_leaves_a_pr_with_no_other_cause_unknown_not_clean(self):
        self.assertEqual(pm.path_of(self.f(), None), (None, []))
        self.assertEqual(pm.path_of(self.f(), self.comp(complete=False)), (None, []))
        self.assertEqual(pm.path_of(self.f(holds=1), None), ("slow", ["hold"]))  # a known cause needs no compute
        self.assertEqual(pm.path_of(self.f(), self.comp(complete=False, red_events=[T(10, "10:30")])), (None, []))  # an incomplete read is not trusted

    def test_hold_and_red_wait_until_the_next_push_or_the_merge(self):
        f = self.f(hold_times=[T(10, "10:00"), T(10, "11:00")], push_times=[T(10, "10:20")])
        self.assertEqual(pm.cause_minutes(f, self.comp())["hold"], 20.0 + 60.0)  # 10:00 to the push, 11:00 to the 12:00 merge
        red = pm.cause_minutes(self.f(push_times=[T(10, "11:30")]), self.comp(red_events=[T(10, "11:00")]))
        self.assertEqual(red["red"], 30.0)

    def test_queue_time_runs_from_each_enqueue_to_its_removal_or_the_merge(self):
        f = self.f(enqueue_times=[T(10, "10:00"), T(10, "10:40")], dequeue_times=[T(10, "10:10")])
        self.assertEqual(pm.cause_minutes(f, self.comp())["queue"], 10.0 + 80.0)

    def test_unmeasured_causes_are_none_not_zero_and_an_unmerged_pr_has_no_causes(self):
        c = pm.cause_minutes(self.f(), None)
        self.assertEqual((c["red"], c["runner"], c["dirty"]), (None, None, None))
        self.assertIsNone(pm.cause_minutes(self.f(merged_at=None), self.comp()))

    def test_the_summary_reports_share_medians_and_the_top_cause(self):
        row = lambda lead, path, why=(), **cm: {"handoff_min": lead, "path": path, "path_why": list(why),
                                                "cause_min": {"hold": 0.0, "red": 0.0, "queue": 0.0, "runner": 0.0, "dirty": None, **cm}}
        rows = [row(30.0, "clean"), row(50.0, "clean"), row(70.0, "clean"), row(400.0, "slow", ["red"], red=120.0), row(100.0, "slow", ["hold"], hold=30.0),
                row(60.0, None, ())]
        p = pm.path_summary(rows)
        self.assertEqual((p["clean"]["n"], p["clean"]["median_min"], p["slow"]["n"], p["slow"]["median_min"], p["slow"]["p90_min"], p["unknown"]),
                         (3, 50.0, 2, 250.0, 400.0, 1))
        self.assertEqual(p["slow_share_pct"], 40.0)  # unknown is in neither path
        cm = p["cause_min"]
        self.assertEqual((p["top_cause"], cm["red"]["mean_min"], cm["hold"]["mean_min"], cm["dirty"]["mean_min"], cm["dirty"]["n"]), ("red", 60.0, 15.0, None, 0))
        self.assertEqual(pm.path_summary([])["top_cause"], None)

    def test_a_cause_is_averaged_over_the_slow_prs_it_was_measured_for_never_over_all_merged_prs(self):
        row = lambda red: {"handoff_min": 100.0, "path": "slow", "path_why": ["hold"], "cause_min": {"hold": 0.0, "red": red, "queue": 0.0, "runner": None, "dirty": None}}
        rows = [row(60.0), row(None), {"handoff_min": 50.0, "path": "clean", "path_why": [], "cause_min": None}]
        cm = pm.path_summary(rows)["cause_min"]
        self.assertEqual((cm["red"]["mean_min"], cm["red"]["n"], cm["red"]["of_slow"]), (60.0, 1, 2))  # not 60 / 3 merged or 60 / 2 slow
        self.assertEqual((cm["runner"]["mean_min"], cm["runner"]["n"]), (None, 0))

    def test_a_commit_date_and_a_force_push_both_after_the_start_make_the_wait_unknown_not_a_guess(self):
        f = self.f(hold_times=[T(10, "10:20")], push_times=[T(10, "10:30"), T(10, "10:50")], push_times_observed=[T(10, "10:50")])
        self.assertIsNone(pm.cause_minutes(f, self.comp())["hold"])  # the repair commit is dated 10:30, its force-push 10:50
        only_forced = self.f(hold_times=[T(10, "10:20")], push_times=[T(10, "10:50")], push_times_observed=[T(10, "10:50")])
        self.assertEqual(pm.cause_minutes(only_forced, self.comp())["hold"], 30.0)

    def test_a_wait_that_ends_at_a_force_push_is_exact_and_one_that_ends_at_a_commit_date_is_flagged(self):
        f = self.f(hold_times=[T(10, "10:00")], push_times=[T(10, "10:20")], push_times_observed=[T(10, "10:20")])
        c = pm.cause_minutes(f, self.comp())
        self.assertEqual((c["hold"], c["estimated_endpoints"]), (20.0, False))
        est = pm.cause_minutes(dict(f, push_times_observed=[]), self.comp())
        self.assertEqual((est["hold"], est["estimated_endpoints"]), (20.0, True))

    def test_the_report_prints_the_clean_and_slow_table(self):
        out = pm.build([REPO], tempfile.mkdtemp(), 50, OWNER, START, NOW, {}, {}, None, world([pr(1, T(10, "10:00"), T(10, "11:00"))]))
        md = pm.markdown(out)
        self.assertIn("### Clean and slow paths (window, merged PRs)", md)
        self.assertIn("| slow share | clean median (n) | slow median / p90 (n) | top cause |", md)


class RerunAttempts(Base):
    HEAD = f"h{1:040d}"[:40]

    def world(self, attempt, prev_conclusion, readable=True):
        p = pr(1, T(10, "09:00"), T(10, "11:00"))
        run = {"id": 11, "name": "ci", "head_sha": self.HEAD, "event": "pull_request", "status": "completed", "conclusion": "success",
               "head_branch": "feat", "run_attempt": attempt, "updated_at": T(10, "10:40")}
        job = [{"labels": ["x"], "created_at": T(10, "10:00"), "started_at": T(10, "10:01"), "completed_at": T(10, "10:05"), "conclusion": "success"}]
        gh = world([p], runs=[run], jobs={11: job})
        if readable:
            gh.routes.insert(0, (r"actions/runs/11/attempts/1", {"conclusion": prev_conclusion, "updated_at": T(10, "10:20")}))
        return gh

    def build_one(self, gh):
        return pm.build([REPO], self.cache, 300, OWNER, START, NOW, {}, {}, None, gh, control=False)["repos"][REPO]["prs"][0]

    def test_a_force_push_to_an_older_commit_ends_the_wait_at_the_force_push_not_at_the_merge(self):
        p = pr(1, T(10, "10:00"), T(10, "12:00"))
        tl = {1: [{"event": "commented", "user": {"login": "r"}, "created_at": T(10, "10:30"), "body": "HOLD: fix"},
                  {"event": "head_ref_force_pushed", "actor": {"login": "x"}, "created_at": T(10, "10:50"), "after_sha": "o" * 40}]}
        cm = {1: [commit("old", "x", "w", date=T(10, "09:00"))]}  # the commit is older than ready: it gives no push time by itself
        out = self.build(world([p], tl, cm))
        r = self.rows(out)[1]
        self.assertEqual(r["cause_min"]["hold"], 20.0)  # 10:30 to the 10:50 force-push, not to the 12:00 merge
        self.assertFalse(r["cause_min"]["estimated_endpoints"])

    def test_every_earlier_attempt_is_read_even_when_the_latest_attempt_failed_too(self):
        p = pr(1, T(10, "09:00"), T(10, "11:00"))
        run = {"id": 11, "name": "ci", "head_sha": self.HEAD, "event": "pull_request", "status": "completed", "conclusion": "failure",
               "head_branch": "feat", "run_attempt": 3, "updated_at": T(10, "10:40")}
        gh = world([p], runs=[run], jobs={11: [{"labels": ["x"], "created_at": T(10, "10:00"), "started_at": T(10, "10:01"), "completed_at": T(10, "10:05"), "conclusion": "failure"}]})
        gh.routes.insert(0, (r"actions/runs/11/attempts/1", {"conclusion": "failure", "updated_at": T(10, "10:10")}))
        gh.routes.insert(0, (r"actions/runs/11/attempts/2", {"conclusion": "failure", "updated_at": T(10, "10:25")}))
        r = self.build_one(gh)
        self.assertEqual(r["compute"]["red_events"], [T(10, "10:10"), T(10, "10:25"), T(10, "10:40")])
        shutil.rmtree(os.path.join(self.cache, REPO.replace("/", "__")))  # the first read was cached
        gh2 = world([p], runs=[run], jobs={11: [{"labels": ["x"], "created_at": T(10, "10:00"), "started_at": T(10, "10:01"), "completed_at": T(10, "10:05"), "conclusion": "failure"}]})
        self.assertFalse(self.build_one(gh2)["compute"]["complete"])  # earlier attempts unreadable: not complete even though the latest failed

    def test_a_failed_first_attempt_followed_by_a_passing_rerun_still_makes_the_pr_red(self):
        r = self.build_one(self.world(2, "failure"))
        self.assertEqual((r["path"], r["path_why"], r["compute"]["red_events"]), ("slow", ["red"], [T(10, "10:20")]))

    def test_a_rerun_whose_first_attempt_passed_is_clean_and_a_first_run_reads_no_attempts(self):
        self.assertEqual(self.build_one(self.world(2, "success"))["path"], "clean")
        gh = self.world(1, "failure")
        self.assertEqual(self.build_one(gh)["path"], "clean")
        self.assertFalse([c for c in gh.calls if "/attempts/" in c])

    def test_unreadable_earlier_attempts_leave_the_pr_unknown_not_clean(self):
        r = self.build_one(self.world(2, "failure", readable=False))
        self.assertEqual((r["compute"]["complete"], r["path"]), (False, None))


class JobsSkippedHold(Base):
    """Findings of the review of the job-read switch."""
    def run_skipped(self, runs, seed=None, jobs=None):
        p = pr(1, T(10, "09:00"), T(10, "11:00"))
        gh = world([p], runs=runs, jobs=jobs or {})
        if seed is not None:
            d = os.path.join(self.cache, REPO.replace("/", "__"), "compute")
            os.makedirs(d)
            with open(os.path.join(d, "1.json"), "w") as fh:
                json.dump(seed, fh)
        pm.READ_JOB_MINUTES = False
        try:
            return pm.build([REPO], self.cache, 200, OWNER, START, NOW, {}, {}, None, gh, control=False)
        finally:
            pm.READ_JOB_MINUTES = True

    RUN = {"id": 11, "head_sha": f"h{1:040d}"[:40], "event": "pull_request", "head_branch": "feat"}

    def test_an_unfinished_run_keeps_the_pr_unknown_when_job_reads_are_skipped(self):
        out = self.run_skipped([dict(self.RUN, status="in_progress")])
        r = out["repos"][REPO]["prs"][0]
        self.assertEqual((r["compute"]["complete"], r["path"]), (False, None))
        done = self.run_skipped([dict(self.RUN, status="completed", conclusion="success")])
        self.assertEqual(done["repos"][REPO]["prs"][0]["path"], "clean")

    def test_skipped_job_reads_report_no_minutes_not_zeros_and_the_summary_copes(self):
        out = self.run_skipped([dict(self.RUN, status="completed", conclusion="success")])
        c = out["repos"][REPO]["prs"][0]["compute"]
        self.assertEqual((c["self_hosted_min"], c["hosted_min"], c["macos_min"], c["runner_wait_min"], c["jobs_read"]), (None, None, None, None, False))
        s = pm.summarize([r for r in out["repos"][REPO]["prs"] if r["eligible"]])["compute_per_merged_pr"]
        self.assertEqual((s["context_self_hosted_min_median"], s["macos_min_mean"], s["macos_min_median"], s["runs_total"]), (None, None, None, 1))

    def test_a_cached_compute_entry_with_minutes_is_not_reused_when_job_reads_are_skipped(self):
        seed = {"v": pm.COMPUTE_VERSION, "complete": True, "jobs_read": True, "self_hosted_min": 99.0, "hosted_min": 1.0, "macos_min": 5.0,
                "runner_wait_min": 2.0, "red_events": [], "by_workflow": {}, "runs": 7, "repeat_runs": 0, "group_runs": 0}
        out = self.run_skipped([dict(self.RUN, status="completed", conclusion="success")], seed)
        c = out["repos"][REPO]["prs"][0]["compute"]
        self.assertEqual((c["self_hosted_min"], c["runs"]), (None, 1))


class JobsSkipped(Base):
    runs = [{"id": 11, "head_sha": f"h{1:040d}"[:40], "event": "pull_request", "status": "completed", "head_branch": "feat"},
            {"id": 12, "head_sha": "zzz", "event": "merge_group", "status": "completed", "head_branch": "gh-readonly-queue/main/pr-1-abc"}]

    def test_without_job_reads_runs_and_failed_head_runs_still_count_and_nothing_is_cached(self):
        runs = [dict(r, conclusion="failure", updated_at=T(10, "10:12")) for r in self.runs]
        gh = world([pr(1, T(10, "09:00"), T(10, "11:00"))], runs=runs, jobs={})  # no jobs route: any jobs read would fail
        pm.READ_JOB_MINUTES = False
        try:
            out = pm.build([REPO], self.cache, 200, OWNER, START, NOW, {}, {}, None, gh, control=False)  # the control reads jobs
        finally:
            pm.READ_JOB_MINUTES = True
        r = self.rows(out)[1]
        self.assertEqual((r["compute"]["runs"], r["compute"]["complete"], r["compute"]["runner_wait_min"], r["path"]), (2, True, None, "slow"))
        self.assertIsNone(r["cause_min"]["runner"])
        self.assertFalse([c for c in gh.calls if "/jobs" in c])
        self.assertFalse(os.path.exists(os.path.join(self.cache, REPO.replace("/", "__"), "compute", "1.json")))

    def test_a_failed_push_run_on_a_pr_head_counts_as_red(self):
        run = dict(self.runs[0], event="push", conclusion="failure", updated_at=T(10, "10:12"))
        gh = world([pr(1, T(10, "09:00"), T(10, "11:00"))], runs=[run], jobs={})
        pm.READ_JOB_MINUTES = False
        try:
            out = pm.build([REPO], self.cache, 200, OWNER, START, NOW, {}, {}, None, gh, control=False)
        finally:
            pm.READ_JOB_MINUTES = True
        r = self.rows(out)[1]
        self.assertEqual((r["path"], r["path_why"]), ("slow", ["red"]))


class MergeRemoval(Base):
    def test_a_removal_after_the_merge_is_not_an_ejection(self):
        p = pr(1, T(10, "10:00"), T(10, "11:00"))
        out = self.build(world([p], {1: [self.removal("11:05")]}))
        self.assertEqual(self.rows(out)[1]["ejections"], [])

    def removal(self, hm):
        return {"event": "removed_from_merge_queue", "actor": {"login": "x"}, "created_at": T(10, hm)}

    def test_the_removal_github_records_at_the_merge_is_not_an_ejection_but_an_earlier_one_is(self):
        p = pr(1, T(10, "10:00"), T(10, "11:00"))
        tl = {1: [self.removal("10:30"), self.removal("10:59"), self.removal("11:00")]}
        out = self.build(world([p], tl))
        r = self.rows(out)[1]
        self.assertEqual(r["ejections"], ["unknown"])  # the 10:30 one only
        self.assertEqual((r["path"], r["path_why"]), ("slow", ["ejection"]))

    def test_a_pr_with_only_the_merge_removal_is_not_slow_for_an_ejection(self):
        p = pr(1, T(10, "10:00"), T(10, "11:00"))
        out = self.build(world([p], {1: [self.removal("11:00")]}))
        r = self.rows(out)[1]
        self.assertEqual((r["ejections"], "ejection" in r["path_why"]), ([], False))


class Correctness(Base):
    def merged_old(self, n, day=5):
        return pr(n, T(day, "09:00"), T(day, "10:00"), sha=f"s{n}" + "0" * 38)

    def test_clean_pr_older_than_seven_days_is_good_and_final(self):
        p = self.merged_old(5)
        bc = [commit(p["merge_commit_sha"], date=T(5, "10:00"))]
        out = self.build(world([p], base_commits=bc, checks={p["merge_commit_sha"]: [{"name": "Test", "status": "completed", "conclusion": "success"}]}),
                         now=dt.datetime(2026, 11, 14, tzinfo=UTC))
        c = self.rows(out)[5]["correct"]
        self.assertEqual((c["bad"], c["provisional"], c["reverted"], c["base_red"]), (False, False, False, False))

    def test_a_revert_naming_the_pr_within_seven_days_counts(self):
        p = self.merged_old(5)
        bc = [commit(p["merge_commit_sha"], date=T(5, "10:00")), commit("r1", msg='Revert "thing (#5)"', date=T(7, "10:00")),
              commit("r2", msg='Revert "thing (#55)"', date=T(7, "11:00"))]
        ck = {p["merge_commit_sha"]: [{"name": "Test", "status": "completed", "conclusion": "success"}], "r1": [{"name": "Test", "status": "completed", "conclusion": "success"}]}
        out = self.build(world([p], base_commits=bc, checks=ck), now=dt.datetime(2026, 11, 14, tzinfo=UTC))
        c = self.rows(out)[5]["correct"]
        self.assertTrue(c["reverted"] and c["bad"])

    def test_a_revert_of_another_pr_number_does_not_match(self):
        p = self.merged_old(5)
        bc = [commit(p["merge_commit_sha"], date=T(5, "10:00")), commit("r2", msg='Revert "thing (#55)"', date=T(7, "11:00"))]
        ck = {p["merge_commit_sha"]: [{"name": "Test", "status": "completed", "conclusion": "success"}], "r2": [{"name": "Test", "status": "completed", "conclusion": "success"}]}
        out = self.build(world([p], base_commits=bc, checks=ck), now=dt.datetime(2026, 11, 14, tzinfo=UTC))
        self.assertFalse(self.rows(out)[5]["correct"]["bad"])

    def test_a_revert_after_the_window_does_not_count(self):
        p = self.merged_old(5)
        bc = [commit(p["merge_commit_sha"], date=T(5, "10:00")), commit("r1", msg="Revert it (#5)", date=T(13, "10:00"))]
        ck = {p["merge_commit_sha"]: [{"name": "Test", "status": "completed", "conclusion": "success"}], "r1": [{"name": "Test", "status": "completed", "conclusion": "success"}]}
        out = self.build(world([p], base_commits=bc, checks=ck), now=dt.datetime(2026, 11, 14, tzinfo=UTC))
        self.assertFalse(self.rows(out)[5]["correct"]["reverted"])

    def test_red_required_check_at_the_merge_commit_or_next_push(self):
        p, q = self.merged_old(6), self.merged_old(7, day=4)
        ok = [{"name": "Test", "status": "completed", "conclusion": "success"}]
        red = [{"name": "Test", "status": "completed", "conclusion": "failure"}, {"name": "Other", "status": "completed", "conclusion": "failure"}]
        bc = [commit(q["merge_commit_sha"], date=T(4, "10:00")), commit("nx", date=T(4, "12:00")),
              commit(p["merge_commit_sha"], date=T(5, "10:00")), commit("ny", date=T(5, "12:00"))]
        ck = {q["merge_commit_sha"]: ok, "nx": red, p["merge_commit_sha"]: red, "ny": ok}
        out = self.build(world([p, q], base_commits=bc, checks=ck), now=dt.datetime(2026, 11, 14, tzinfo=UTC))
        r = self.rows(out)
        self.assertTrue(r[6]["correct"]["base_red"])  # red at its merge commit
        self.assertTrue(r[7]["correct"]["base_red"])  # clean merge commit, red at the next push
        s = out["repos"][REPO]["per_day"]
        self.assertEqual(s["2026-11-05"]["correctness"]["bad_rate_pct"], 100.0)

    def test_a_non_required_red_check_is_ignored(self):
        p = self.merged_old(6)
        bc = [commit(p["merge_commit_sha"], date=T(5, "10:00"))]
        ck = {p["merge_commit_sha"]: [{"name": "Test", "status": "completed", "conclusion": "success"}, {"name": "Lint", "status": "completed", "conclusion": "failure"}]}
        out = self.build(world([p], base_commits=bc, checks=ck), now=dt.datetime(2026, 11, 14, tzinfo=UTC))
        self.assertFalse(self.rows(out)[6]["correct"]["bad"])

    def test_tick_base_poison_episode_counts_without_reading_check_runs(self):
        p = self.merged_old(6)
        bc = [commit(p["merge_commit_sha"], date=T(5, "10:00"))]
        ticks = {REPO: [dt.datetime(2026, 11, 5, 10, 30, tzinfo=UTC)]}
        gh = world([p], base_commits=bc)
        out = self.build(gh, ticks=ticks, now=dt.datetime(2026, 11, 14, tzinfo=UTC))
        c = self.rows(out)[6]["correct"]
        self.assertEqual((c["bad"], c["base_red_source"]), (True, "ticks"))
        self.assertFalse([x for x in gh.calls if "check-runs" in x])

    def test_a_pr_under_seven_days_old_is_provisional_and_flagged(self):
        p = pr(8, T(12, "09:00"), T(12, "10:00"), sha="s8" + "0" * 38)
        bc = [commit(p["merge_commit_sha"], date=T(12, "10:00"))]
        ck = {p["merge_commit_sha"]: [{"name": "Test", "status": "completed", "conclusion": "success"}]}
        out = self.build(world([p], base_commits=bc, checks=ck))
        c = self.rows(out)[8]["correct"]
        self.assertTrue(c["provisional"])
        k = out["repos"][REPO]["per_day"]["2026-11-12"]["correctness"]
        self.assertEqual(k["provisional_prs"], 1)

    def test_an_incomplete_multi_page_check_read_is_not_settled_or_cached(self):
        p = self.merged_old(6)
        greens = [{"name": f"j{i}", "status": "completed", "conclusion": "success"} for i in range(200)]
        ck = {p["merge_commit_sha"]: lambda path: greens[:100] if path.endswith("&page=1") else greens[100:]}
        out = self.build(world([p], base_commits=[commit(p["merge_commit_sha"], date=T(5, "10:00"))], checks=ck),
                         now=dt.datetime(2026, 11, 14, tzinfo=UTC))
        c = self.rows(out)[6]["correct"]
        self.assertEqual((c["base_red"], c["bad"]), (None, None))
        self.assertFalse(os.path.exists(os.path.join(self.cache, REPO.replace("/", "__"), "checks", p["merge_commit_sha"] + ".json")))

    def test_old_unversioned_clean_caches_are_read_again(self):
        p = self.merged_old(6)
        root = os.path.join(self.cache, REPO.replace("/", "__"))
        sha = p["merge_commit_sha"]
        for kind, name, body in (("checks", sha, {"red": []}),
                                 ("correct", "6", {"provisional": False, "reverted": False, "base_red": False, "bad": False,
                                                   "escape_provisional": False, "escaped": False, "escaped_by": None})):
            os.makedirs(os.path.join(root, kind), exist_ok=True)
            with open(os.path.join(root, kind, name + ".json"), "w") as fh:
                json.dump(body, fh)
        red = [{"name": "Test", "status": "completed", "conclusion": "failure"}]
        gh = world([p], base_commits=[commit(sha, date=T(5, "10:00"))], checks={sha: red})
        c = self.rows(self.build(gh, now=dt.datetime(2026, 11, 14, tzinfo=UTC)))[6]["correct"]
        self.assertTrue([x for x in gh.calls if "check-runs" in x])
        self.assertEqual((c["base_red"], c["bad"]), (True, True))

    def test_unreadable_checks_make_correctness_unknown(self):
        p = self.merged_old(6)
        out = self.build(world([p], base_commits=[commit(p["merge_commit_sha"], date=T(5, "10:00"))], default_checks=False), now=dt.datetime(2026, 11, 14, tzinfo=UTC))
        k = out["repos"][REPO]["per_day"]["2026-11-05"]["correctness"]
        self.assertEqual((k["unknown"], k["bad_rate_pct"]), (1, None))


class Cohort(Base):
    def test_excluded_labels_and_unmanaged_bases_leave_the_cohort_with_a_reason(self):
        a, b, c = (pr(i, T(10, "10:00"), T(10, "11:00")) for i in (1, 2, 3))
        a["labels"] = [{"name": "automation"}]
        b["labels"] = [{"name": "shipyard:unmanaged"}]
        c["base"] = {"ref": "feature-x"}
        d = pr(4, T(10, "10:00"), T(10, "11:00"))
        out = self.build(world([a, b, c, d]))
        v = out["repos"][REPO]
        self.assertEqual(v["excluded"], {"automation": 1, "unmanaged": 1, "unmanaged_base": 1})
        self.assertEqual(v["per_day"]["2026-11-10"]["merged"], 1)

    def test_a_pr_is_assigned_to_the_day_it_became_ready_not_the_day_it_merged(self):
        out = self.build(world([pr(1, T(10, "22:00"), T(11, "02:00"))]))
        r = self.rows(out)[1]
        self.assertEqual((r["day"], r["merged_at"][:10]), ("2026-11-10", "2026-11-11"))
        self.assertIn("2026-11-10", out["repos"][REPO]["per_day"])

    def test_owner_actions_are_counted_apart_from_repair_cycles_and_holds(self):
        bot = {"login": "shipyard-local[bot]"}
        tl = {1: [{"event": "commented", "actor": bot, "body": "HOLD: red", "created_at": T(10, "10:10")},
                  {"event": "commented", "actor": bot, "body": "repaired: fix at abc", "created_at": T(10, "10:20")},
                  {"event": "commented", "actor": bot, "body": "repaired: again", "created_at": T(10, "10:30")},
                  {"event": "commented", "actor": bot, "body": "verdict: HOLD head: abc", "created_at": T(10, "10:35")},
                  {"event": "labeled", "actor": {"login": OWNER}, "created_at": T(10, "10:40")},
                  {"event": "commented", "actor": {"login": OWNER}, "body": "ok", "created_at": T(10, "10:50")}]}
        out = self.build(world([pr(1, T(10, "10:00"), T(10, "11:00"))], tl))
        r = self.rows(out)[1]
        self.assertEqual((r["owner_actions"], r["repair_cycles"], r["holds"], r["re_holds"], r["size"]), (2, 2, 2, 1, 400))
        day = out["repos"][REPO]["per_day"]["2026-11-10"]
        iv = day["interventions"]
        self.assertEqual((iv["owner_actions_per_pr"], iv["repair_cycles_per_pr"], iv["prs_with_repair_cycle"]), (2.0, 2.0, 1))
        self.assertEqual((day["rounds"]["re_hold_rate_pct"], day["rounds"]["by_size"]), (100.0, {"200-800": {"prs_with_hold": 1, "prs_with_re_hold": 1}}))


class Escapes(Base):
    def test_a_later_pr_naming_this_one_as_the_cause_within_14_days_is_an_escape(self):
        p1 = pr(5, T(1, "09:00"), T(1, "10:00"))
        fix = pr(9, T(3, "09:00"), T(3, "10:00"))
        fix["title"], fix["body"] = "fix crash", "A regression from #5 broke the queue"
        late = pr(10, T(20, "09:00"), T(20, "10:00"))
        late["title"] = "fix typo from #5"  # 19 days later: outside the window
        other = pr(11, T(3, "11:00"), T(3, "12:00"))
        other["title"], other["body"] = "fix thing", "unrelated to #55"
        out = self.build(world([p1, fix, late, other]), now=dt.datetime(2026, 11, 30, tzinfo=UTC))
        c = self.rows(out)[5]["correct"]
        self.assertEqual((c["escaped"], c["escaped_by"], c["escape_provisional"]), (True, 9, False))
        self.assertFalse(self.rows(out)[11]["correct"]["escaped"])
        k = out["repos"][REPO]["per_day"]["2026-11-01"]["correctness"]
        self.assertEqual(k["escaped"], 1)

    def test_escape_window_is_provisional_for_a_pr_under_14_days_old(self):
        out = self.build(world([pr(5, T(10, "09:00"), T(10, "10:00"))]))
        self.assertTrue(self.rows(out)[5]["correct"]["escape_provisional"])


class Attribution(Base):
    """Agents commit under the owner's identity, so authorship comes from stronger signals."""

    def run_one(self, p=None, tl=None, cm=None):
        p = p or pr(1, T(10, "10:00"), T(10, "11:00"))
        shutil.rmtree(os.path.join(self.cache, REPO.replace("/", "__")), ignore_errors=True)  # each call reads fresh
        out = self.build(world([p], {1: tl or []}, {1: cm or [commit("c1", OWNER, "work")]}))
        return self.rows(out)[1], out

    def opened(self, login=None, body=None):
        p = pr(1, T(10, "10:00"), T(10, "11:00"))
        p["user"], p["body"] = {"login": login or OWNER}, body
        return p

    def test_a_human_only_pr_stays_unknown_and_a_human_action_stays_owner(self):
        r, _ = self.run_one(self.opened())
        self.assertEqual((r["north"], r["attribution"]["signals"], r["attribution"]["primary"]), ("unknown", [], None))
        tl = [{"event": "labeled", "actor": {"login": OWNER}, "created_at": T(10, "10:30")}]
        r, _ = self.run_one(self.opened(), tl)
        self.assertEqual(r["north"], "owner_action")

    def test_opened_by_the_fleet_app_clears_owner_identity_commits(self):
        r, out = self.run_one(self.opened("shipyard-local[bot]"))
        self.assertEqual((r["north"], r["attribution"]["primary"], r["attribution"]["legacy_north"]), ("autonomous", "app_opener", "unknown"))
        a = out["repos"][REPO]["per_day"]["2026-11-10"]["attribution"]
        self.assertEqual((a["by_primary"]["app_opener"], a["ambiguous_cleared"]["app_opener"], a["legacy"]["unknown"]), (1, 1, 1))

    def test_shipyard_provenance_markers_in_the_body(self):
        for body in ("x\n<!-- shipyard-owner: host=m5 client=codex -->", "<!-- whence {\"a\":1} -->"):
            r, _ = self.run_one(self.opened(body=body))
            self.assertEqual((r["north"], r["attribution"]["primary"]), ("autonomous", "provenance"))

    def test_agent_trailer_on_any_commit(self):
        cm = [commit("c1", OWNER, "work"), commit("c2", "someone", "x\n\nCo-Authored-By: Claude <noreply@anthropic.com>")]
        r, _ = self.run_one(self.opened(), cm=cm)
        self.assertEqual((r["north"], r["attribution"]["primary"]), ("autonomous", "trailer"))

    def test_session_stamp_in_the_body_or_a_commit(self):
        r, _ = self.run_one(self.opened(body="<!-- session: m5/abc -->"))
        self.assertEqual((r["north"], r["attribution"]["primary"]), ("autonomous", "session_stamp"))
        r, _ = self.run_one(self.opened(), cm=[commit("c1", OWNER, "work\n\n<!-- session: m5/abc -->")])
        self.assertEqual((r["north"], r["attribution"]["primary"]), ("autonomous", "session_stamp"))

    def test_signals_are_all_listed_and_the_strongest_is_primary(self):
        p = self.opened("shipyard-local[bot]", "<!-- whence {} -->")
        r, _ = self.run_one(p, cm=[commit("c1", OWNER, "w\n\nCo-Authored-By: Claude <a@anthropic.com>")])
        self.assertEqual(r["attribution"]["signals"], ["app_opener", "provenance", "trailer"])
        self.assertEqual(r["attribution"]["primary"], "app_opener")

    def test_an_owner_comment_with_a_session_stamp_is_an_agent_not_an_owner_action(self):
        tl = [{"event": "commented", "user": {"login": OWNER}, "created_at": T(10, "10:30"), "body": "repaired: x\n<!-- session: m5/abc -->"}]
        r, out = self.run_one(self.opened(), tl)
        self.assertEqual((r["north"], r["attribution"]["agent_owner_events"], r["attribution"]["primary"]), ("autonomous", 1, "session_stamp"))
        self.assertEqual(out["repos"][REPO]["per_day"]["2026-11-10"]["attribution"]["agent_stamped_owner_events"], 1)

    MERGE = {"event": "merged", "actor": {"login": OWNER}, "created_at": T(10, "11:00")}

    def app_comment(self, body, login="shipyard-local[bot]"):
        return {"event": "commented", "user": {"login": login}, "created_at": T(10, "10:30"), "body": body}

    def merged_pr(self, login=OWNER):
        p = self.opened(login)
        p["head"] = {"sha": HEAD}
        return p

    APPROVE = ("reviewer: rev\n<!-- session: m3/fleet-pass-reviewer-7 -->\nverdict: APPROVE head:" + HEAD)

    def test_a_trusted_app_approve_with_a_session_stamp_makes_an_owner_merge_a_steward_merge(self):
        r, _ = self.run_one(self.merged_pr(), [self.app_comment(self.APPROVE), self.MERGE], cm=[commit("c1", "x", "w")])
        self.assertEqual((r["north"], r["attribution"]["steward_merge_recognised"], r["attribution"]["legacy_north"]), ("autonomous", True, "owner_action"))

    def test_an_app_enqueue_event_makes_an_owner_merge_a_steward_merge(self):
        enq = {"event": "added_to_merge_queue", "actor": {"login": "shipyard-local[bot]"}, "created_at": T(10, "10:40")}
        r, _ = self.run_one(self.merged_pr(), [self.CM, enq, self.MERGE], cm=[commit("c1", "x", "w")])
        self.assertEqual((r["north"], r["attribution"]["steward_merge_recognised"]), ("autonomous", True))

    def enq(self, hm, kind="added_to_merge_queue", login="shipyard-local[bot]"):
        return {"event": kind, "actor": {"login": login}, "created_at": T(10, hm)}

    def test_an_ejected_enqueue_followed_by_a_manual_merge_is_an_owner_action(self):
        tl = [self.CM, self.enq("10:40"), dict(self.enq("10:45", "removed_from_merge_queue"), reason="failed_checks"), self.MERGE]
        r, _ = self.run_one(self.merged_pr(), tl, cm=[commit("c1", "x", "w")])
        self.assertEqual((r["north"], r["attribution"]["steward_merge_recognised"]), ("owner_action", False))

    def test_an_enqueue_then_a_hold_then_a_manual_merge_is_an_owner_action(self):
        hold = dict(self.app_comment("verdict: HOLD head:" + HEAD), created_at=T(10, "10:50"))
        r, _ = self.run_one(self.merged_pr(), [self.CM, self.enq("10:40"), hold, self.MERGE], cm=[commit("c1", "x", "w")])
        self.assertEqual(r["north"], "owner_action")

    def test_a_new_enqueue_after_an_ejection_that_leads_to_the_merge_is_a_steward_merge(self):
        tl = [self.CM, self.enq("10:40"), dict(self.enq("10:45", "removed_from_merge_queue"), reason="failed_checks"), self.enq("10:50"), self.MERGE]
        r, _ = self.run_one(self.merged_pr(), tl, cm=[commit("c1", "x", "w")])
        self.assertEqual((r["north"], r["attribution"]["steward_merge_recognised"]), ("autonomous", True))

    CM = {"event": "committed", "sha": HEAD}

    def test_a_clock_skewed_commit_timestamp_does_not_change_the_classification(self):
        for date in (T(1, "00:00"), T(10, "10:50"), T(30, "23:00")):
            r, _ = self.run_one(self.merged_pr(), [self.CM, self.enq("10:40"), self.MERGE], cm=[commit("c1", "x", "w", date=date)])
            self.assertEqual((r["north"], r["attribution"]["steward_merge_recognised"]), ("autonomous", True))

    def test_a_later_push_of_an_older_commit_after_the_enqueue_is_never_steward(self):
        older = {"event": "committed", "sha": "c" * 40}
        r, _ = self.run_one(self.merged_pr(), [self.CM, self.enq("10:40"), older, self.MERGE], cm=[commit("c1", "x", "w", date=T(1, "00:00"))])
        self.assertEqual((r["attribution"]["steward_merge_recognised"], r["north"]), (False, "owner_action"))
        forced = {"event": "head_ref_force_pushed", "actor": {"login": "x"}, "created_at": T(1, "00:00"), "after_sha": HEAD}
        r, _ = self.run_one(self.merged_pr(), [self.CM, self.enq("10:40"), forced, self.MERGE], cm=[commit("c1", "x", "w")])
        self.assertEqual(r["attribution"]["steward_merge_recognised"], False)

    def test_an_enqueue_the_timeline_cannot_bind_to_the_merged_sha_is_unknown_and_counted_apart(self):
        # no commit event at all, or the head then was another SHA with no later push
        for tl in ([self.enq("10:40"), self.MERGE], [{"event": "committed", "sha": "d" * 40}, self.enq("10:40"), self.MERGE]):
            r, out = self.run_one(self.merged_pr(), tl, cm=[commit("c1", "x", "w")])
            self.assertEqual((r["north"], r["attribution"]["steward_merge_recognised"], r["attribution"]["merge_unverified"]), ("unknown", False, True))
            a = out["repos"][REPO]["per_day"]["2026-11-10"]["attribution"]
            self.assertEqual((a["owner_merges_unverified"], a["steward_merges_recognised"]), (1, 0))
            ns = out["repos"][REPO]["per_day"]["2026-11-10"]["north_star"]
            self.assertEqual((ns["owner_action"], ns["autonomous"], ns["unknown"]), (0, 0, 1))
            self.assertEqual(r["owner_actions"], 0)
            iv = out["repos"][REPO]["per_day"]["2026-11-10"]["interventions"]
            self.assertEqual((iv["owner_actions_per_pr"], iv["prs_with_owner_action"]), (0.0, 0))

    def test_an_unverified_merge_from_a_partial_read_is_not_an_owner_action(self):
        gh = world([self.merged_pr()], {1: [self.CM, self.enq("10:40"), self.MERGE]}, {1: self.long_commits()})
        r = self.rows(self.build(gh))[1]
        self.assertEqual((r["north"], r["owner_actions"]), ("unknown", 0))

    def test_an_old_compute_cache_entry_is_recomputed(self):
        d = os.path.join(self.cache, REPO.replace("/", "__"), "compute")
        os.makedirs(d)
        with open(os.path.join(d, "1.json"), "w") as fh:  # written before COMPUTE_VERSION: complete=true from a partial read
            json.dump({"self_hosted_min": 99.0, "hosted_min": 0.0, "runs": 1, "repeat_runs": 0, "group_runs": 0, "complete": True}, fh)
        gh = world([self.merged_pr()], {1: [self.CM, self.enq("10:40"), self.MERGE]}, {1: [commit("c1", "x", "w")]})
        c = self.rows(self.build(gh))[1]["compute"]
        self.assertNotEqual((c or {}).get("self_hosted_min"), 99.0)
        self.assertTrue([x for x in gh.calls if "runs" in x])

    def test_the_report_states_the_actual_steward_evidence_rule(self):
        md = pm.markdown(self.build(world([self.merged_pr()])))
        self.assertIn("trusted evidence bound to the merged SHA", md)
        self.assertIn("unverified owner merge", md)
        for phrase in ("timeline order, never by date", "exact-head APPROVE", "reviewer_signers", "reviewer_stamp_sessions",
                       "left out of the owner-action counts"):
            self.assertIn(phrase, md)
        self.assertNotIn("(or opened through `shipyard pr`) is a steward merge", md)

    def test_an_approval_followed_by_an_ejection_is_not_evidence(self):
        gone = dict(self.enq("10:45", "removed_from_merge_queue"), reason="failed_checks")
        r, _ = self.run_one(self.merged_pr(), [self.app_comment(self.APPROVE), gone, self.MERGE], cm=[commit("c1", "x", "w")])
        self.assertEqual(r["north"], "owner_action")

    def test_a_stamp_removed_from_the_body_does_not_persist_in_a_warm_cache(self):
        cm = {1: [commit("c1", OWNER, "work")]}
        p = self.opened(body="<!-- session: m5/abc -->")
        warm = self.build(world([p], {1: []}, cm))
        self.assertEqual(self.rows(warm)[1]["north"], "autonomous")
        q = self.opened(body="plain")
        gh = world([q], {1: []}, cm)
        out = self.build(gh)  # same cache dir: the entry is warm
        self.assertFalse([c for c in gh.calls if "timeline" in c])  # served from the cache
        self.assertEqual(self.rows(out)[1]["north"], "unknown")
        self.assertEqual(self.rows(out)[1]["attribution"]["signals"], [])

    def many_commits(self):
        return [commit("c1", "x", "w")] + [commit(f"p{i}", "x", "w") for i in range(299)]  # 3 full pages: the read is incomplete

    def test_an_incomplete_commit_read_never_trusts_steward_evidence(self):
        enq = {"event": "added_to_merge_queue", "actor": {"login": "shipyard-local[bot]"}, "created_at": T(10, "10:40")}
        r, _ = self.run_one(self.merged_pr(), [enq, self.MERGE], cm=self.many_commits())
        self.assertFalse(r["attribution"]["steward_merge_recognised"])

    def test_a_pr_read_from_incomplete_commits_is_not_cached_and_is_read_again(self):
        p = self.merged_pr()
        self.build(world([p], {1: [self.MERGE]}, {1: self.many_commits()}))
        gh = world([p], {1: [self.MERGE]}, {1: self.many_commits()})
        self.build(gh)
        self.assertTrue([c for c in gh.calls if "pulls/1/commits" in c])  # not served from the cache

    def test_a_cached_incomplete_entry_is_read_again(self):
        p = self.merged_pr()
        self.build(world([p], {1: [self.MERGE]}, {1: [commit("c1", "x", "w")]}))
        path = os.path.join(self.cache, REPO.replace("/", "__"), "facts", "1.json")
        d = json.load(open(path))
        d["commits_complete"] = False
        json.dump(d, open(path, "w"))
        gh = world([p], {1: [self.MERGE]}, {1: [commit("c1", "x", "w")]})
        self.build(gh)
        self.assertTrue([c for c in gh.calls if "pulls/1/commits" in c])

    def review(self, body, hm="10:30", rid=11, state="commented"):
        return {"event": "reviewed", "id": rid, "state": state, "user": {"login": "shipyard-local[bot]"}, "submitted_at": T(10, hm), "body": body}

    def test_a_review_based_approve_then_an_enqueue_then_a_merge_is_a_steward_merge(self):
        r, _ = self.run_one(self.merged_pr(), [self.review(self.APPROVE), self.CM, self.enq("10:40"), self.MERGE], cm=[commit("c1", "x", "w")])
        self.assertEqual((r["north"], r["attribution"]["steward_merge_recognised"]), ("autonomous", True))
        r, _ = self.run_one(self.merged_pr(), [self.review(self.APPROVE), self.MERGE], cm=[commit("c1", "x", "w")])
        self.assertEqual(r["attribution"]["steward_merge_recognised"], True)  # the approval alone counts too

    def test_a_review_based_hold_after_an_approval_then_a_manual_merge_is_an_owner_action(self):
        hold = self.review("<!-- session: m3/fleet-pass-reviewer-7 -->\nverdict: HOLD head:" + HEAD, "10:50", 12, "changes_requested")
        r, _ = self.run_one(self.merged_pr(), [self.review(self.APPROVE), hold, self.MERGE], cm=[commit("c1", "x", "w")])
        self.assertEqual((r["north"], r["attribution"]["steward_merge_recognised"]), ("owner_action", False))
        # a comment approval cancelled by a later review HOLD
        r, _ = self.run_one(self.merged_pr(), [self.app_comment(self.APPROVE), hold, self.MERGE], cm=[commit("c1", "x", "w")])
        self.assertEqual(r["north"], "owner_action")

    def test_a_review_verdict_naming_the_wrong_head_is_not_counted(self):
        r, _ = self.run_one(self.merged_pr(), [self.review(self.APPROVE.replace(HEAD, "b" * 40)), self.MERGE], cm=[commit("c1", "x", "w")])
        self.assertEqual(r["north"], "owner_action")

    def test_a_review_verdict_from_an_untrusted_user_or_without_a_stamp_is_not_counted(self):
        bad = dict(self.review(self.APPROVE), user={"login": "someone"})
        r, _ = self.run_one(self.merged_pr(), [bad, self.MERGE], cm=[commit("c1", "x", "w")])
        self.assertEqual(r["north"], "owner_action")
        r, _ = self.run_one(self.merged_pr(), [self.review("verdict: APPROVE head:" + HEAD), self.MERGE], cm=[commit("c1", "x", "w")])
        self.assertEqual(r["north"], "owner_action")

    def test_a_dismissed_review_approval_does_not_count(self):
        dis = {"event": "review_dismissed", "actor": {"login": "x"}, "created_at": T(10, "10:45"), "dismissed_review": {"review_id": 11}}
        r, _ = self.run_one(self.merged_pr(), [self.review(self.APPROVE), dis, self.MERGE], cm=[commit("c1", "x", "w")])
        self.assertEqual(r["north"], "owner_action")

    NOISE = {"event": "subscribed", "actor": {"login": "x"}, "created_at": T(10, "10:00")}

    def long_timeline(self, head_events, pages=5):
        """`pages` full pages of 100: the read stops at its page cap and is incomplete."""
        first = head_events + [self.NOISE] * (100 - len(head_events))
        return lambda path: first if path.endswith("&page=1") else [self.NOISE] * 100

    def long_commits(self, pages=3):
        mk = lambda i: commit(f"k{i}", "x", "w", date=T(10, "09:00"))
        return lambda path: [mk(i) for i in range(100)]

    def test_an_incomplete_timeline_is_not_cached_and_is_read_again(self):
        p = self.merged_pr()
        tl = {1: self.long_timeline([self.CM, self.enq("10:40"), self.MERGE])}
        gh = world([p], tl, {1: [commit("c1", "x", "w")]})
        out = self.build(gh)
        self.assertFalse(os.path.exists(os.path.join(self.cache, REPO.replace("/", "__"), "facts", "1.json")))
        gh2 = world([p], tl, {1: [commit("c1", "x", "w")]})
        self.build(gh2)
        self.assertTrue([c for c in gh2.calls if "timeline" in c])

    def test_a_legacy_cache_entry_from_a_partial_read_is_read_again(self):
        old = {"v": pm.FACTS_VERSION, "number": 1, "base": "main", "created_at": T(10, "10:00"), "merged_at": T(10, "11:00"), "state": "closed",
               "merge_commit_sha": "m", "first_ready": T(10, "10:00"), "owner_events": [], "agent_events": [], "steward_seq": [],
               "ambiguous_commits": [], "repair_cycles": 0, "holds": 0, "re_holds": 0, "size": None,
               "pushes": {"repair": 0, "author": 0, "merge_main": 0}, "enqueues": 0, "ejections": [], "labels": [], "heads": ["h"],
               "timeline_complete": False, "commits_complete": True, "agent_trailer": False, "stamp_commit": False}
        d = os.path.join(self.cache, REPO.replace("/", "__"), "facts")
        os.makedirs(d)
        with open(os.path.join(d, "1.json"), "w") as fh:
            json.dump(old, fh)
        gh = world([self.merged_pr()], {1: []}, {1: [commit("c1", "x", "w")]})
        self.build(gh)
        self.assertTrue([c for c in gh.calls if "timeline" in c])

    def test_an_incomplete_timeline_with_an_apparent_steward_path_is_not_a_steward_merge(self):
        tl = {1: self.long_timeline([self.CM, self.enq("10:40"), self.MERGE])}
        r = self.rows(self.build(world([self.merged_pr()], tl, {1: [commit("c1", "x", "w")]})))[1]
        self.assertEqual((r["attribution"]["steward_merge_recognised"], r["north"], r["owner_actions"]), (False, "unknown", 0))
        # the same events read in full are a steward merge
        r, _ = self.run_one(self.merged_pr(), [self.CM, self.enq("10:40"), self.MERGE], cm=[commit("c1", "x", "w")])
        self.assertEqual((r["attribution"]["steward_merge_recognised"], r["north"]), (True, "autonomous"))

    def test_incomplete_commits_with_an_apparent_steward_path_are_not_a_steward_merge(self):
        gh = world([self.merged_pr()], {1: [self.CM, self.enq("10:40"), self.MERGE]}, {1: self.long_commits()})
        r = self.rows(self.build(gh))[1]
        self.assertEqual((r["attribution"]["steward_merge_recognised"], r["north"]), (False, "unknown"))

    def test_a_partial_read_does_not_hide_a_definite_owner_action(self):
        label = {"event": "labeled", "actor": {"login": OWNER}, "created_at": T(10, "10:30")}
        tl = {1: self.long_timeline([label, self.CM, self.enq("10:40"), self.MERGE])}
        r = self.rows(self.build(world([self.merged_pr()], tl, {1: [commit("c1", "x", "w")]})))[1]
        self.assertEqual((r["north"], r["owner_actions"]), ("owner_action", 1))  # the label counts, the unplaced merge does not

    def test_compute_from_partial_heads_is_not_complete(self):
        f = {"heads": ["h"], "base": "main", "number": 1, "timeline_complete": True, "commits_complete": False}
        c = pm.pr_compute(None, f, [], True)
        self.assertFalse(c["complete"])

    def test_an_enqueue_by_anyone_but_the_app_is_not_evidence(self):
        enq = {"event": "added_to_merge_queue", "actor": {"login": "someone"}, "created_at": T(10, "10:40")}
        r, _ = self.run_one(self.merged_pr(), [enq, self.MERGE], cm=[commit("c1", "x", "w")])
        self.assertEqual(r["north"], "owner_action")

    def test_an_owner_merge_after_a_hold_stays_owner_action(self):
        hold = self.app_comment("<!-- session: m3/fleet-pass-reviewer-7 -->\nverdict: HOLD head:" + HEAD)
        r, _ = self.run_one(self.merged_pr(), [hold, self.MERGE], cm=[commit("c1", "x", "w")])
        self.assertEqual((r["north"], r["attribution"]["steward_merge_recognised"]), ("owner_action", False))
        # an approval does not outlive an App HOLD at the same head
        r, _ = self.run_one(self.merged_pr(), [self.app_comment(self.APPROVE), hold, self.MERGE], cm=[commit("c1", "x", "w")])
        self.assertEqual(r["north"], "owner_action")

    def test_an_untrusted_user_posting_an_approve_does_not_make_a_steward_merge(self):
        for body, login in ((self.APPROVE, "someone"), (self.APPROVE, OWNER),
                            ("verdict: APPROVE head:" + HEAD, "shipyard-local[bot]"),  # unsigned, unstamped
                            ("reviewer: nobody\n<!-- session: m3/fleet-pass-reviewer-7 -->\nverdict: APPROVE head:" + HEAD, "shipyard-local[bot]"),
                            ("reviewer: rev\n<!-- session: m3/some-lane -->\nverdict: APPROVE head:" + HEAD, "shipyard-local[bot]"),
                            (self.APPROVE.replace(HEAD, "b" * 40), "shipyard-local[bot]")):  # not the merged head
            r, _ = self.run_one(self.merged_pr(), [self.app_comment(body, login), self.MERGE], cm=[commit("c1", "x", "w")])
            self.assertEqual((r["north"], body[:30]), ("owner_action", body[:30]))

    def test_a_pr_with_no_readable_policy_trusts_no_approval(self):
        gh = world([self.merged_pr()], {1: [self.app_comment(self.APPROVE), self.MERGE]}, {1: [commit("c1", "x", "w")]})
        gh.routes = [x for x in gh.routes if "contents" not in x[0]]
        out = self.build(gh)
        self.assertEqual(self.rows(out)[1]["north"], "owner_action")
        self.assertEqual(out["repos"][REPO]["read_errors"], 0)

    def test_app_opened_alone_is_not_evidence_for_an_owner_merge(self):
        r, _ = self.run_one(self.opened("shipyard-local[bot]"), [self.MERGE])
        self.assertEqual(r["north"], "owner_action")

    def test_owner_merge_with_no_steward_path_and_no_signal_is_a_human_merge(self):
        merge = {"event": "merged", "actor": {"login": OWNER}, "created_at": T(10, "11:00")}
        r, _ = self.run_one(self.opened(), [merge], cm=[commit("c1", "x", "w")])
        self.assertEqual((r["north"], r["attribution"]["steward_merge_recognised"]), ("owner_action", False))

    def test_an_owner_label_still_counts_on_an_app_opened_pr(self):
        tl = [{"event": "labeled", "actor": {"login": OWNER}, "created_at": T(10, "10:30")}]
        r, _ = self.run_one(self.opened("shipyard-local[bot]"), tl)
        self.assertEqual(r["north"], "owner_action")

    def test_a_cached_v3_entry_is_read_again_and_attributed_by_its_trailer(self):
        old = {"v": 3, "number": 1, "base": "main", "created_at": T(10, "10:00"), "merged_at": T(10, "11:00"), "state": "closed",
               "merge_commit_sha": "m", "first_ready": T(10, "10:00"), "owner_events": [], "ambiguous_commits": [T(10, "10:10")],
               "repair_cycles": 0, "holds": 0, "re_holds": 0, "size": None, "pushes": {"repair": 0, "author": 0, "merge_main": 0},
               "enqueues": 0, "ejections": [], "labels": [], "heads": ["h"], "timeline_complete": True, "commits_complete": True}
        d = os.path.join(self.cache, REPO.replace("/", "__"), "facts")
        os.makedirs(d)
        with open(os.path.join(d, "1.json"), "w") as fh:
            json.dump(old, fh)
        cm = {1: [commit("c1", OWNER, "w\n\nCo-Authored-By: Claude <a@anthropic.com>")]}
        gh = world([self.opened()], commits=cm)
        r = self.rows(self.build(gh))[1]
        self.assertTrue([c for c in gh.calls if "commits" in c])
        self.assertEqual((r["north"], r["attribution"]["primary"]), ("autonomous", "trailer"))

    def test_discussion_text_with_verdict_is_not_a_steward_path(self):
        merge = {"event": "merged", "actor": {"login": OWNER}, "created_at": T(10, "11:00")}
        talk = {"event": "commented", "user": {"login": "someone"}, "created_at": T(10, "10:30"), "body": "No verdict: tests pending"}
        r, _ = self.run_one(self.opened(), [talk, merge], cm=[commit("c1", "x", "w")])
        self.assertEqual((r["north"], r["attribution"]["steward_merge_recognised"]), ("owner_action", False))

    def test_the_legacy_rule_counts_stamped_owner_events_as_owner_actions(self):
        tl = [{"event": "commented", "user": {"login": OWNER}, "created_at": T(10, "10:30"), "body": "<!-- session: m5/abc -->"}]
        r, _ = self.run_one(self.opened(), tl)
        self.assertEqual((r["north"], r["attribution"]["legacy_north"]), ("autonomous", "owner_action"))
        r, _ = self.run_one(self.opened(), [], cm=[commit("c1", OWNER, "work")])
        self.assertEqual(r["attribution"]["legacy_north"], "unknown")


def unmerged(n, created, state="open", draft=False):
    p = pr(n, created, None)
    p.update({"merged_at": None, "merge_commit_sha": None, "state": state, "draft": draft, "updated_at": created})
    return p


class NorthStar(Base):
    LATE = dt.datetime(2026, 11, 20, tzinfo=UTC)

    def test_unmerged_ready_prs_stay_in_the_denominator(self):
        prs = [pr(1, T(10, "10:00"), T(10, "11:00")), unmerged(2, T(10, "10:00")), pr(3, T(10, "10:00"), T(13, "10:00"))]  # #3 merged after 48h
        ns = self.build(world(prs), now=self.LATE)["repos"][REPO]["per_day"]["2026-11-10"]["north_star"]
        self.assertEqual((ns["autonomous"], ns["not_merged_in_48h"], ns["rate_pct"]), (1, 2, 33.3))

    def test_an_owner_action_after_the_merge_does_not_count_but_one_before_it_does(self):
        late = {"event": "commented", "actor": {"login": OWNER}, "created_at": T(10, "12:00")}
        early = {"event": "commented", "actor": {"login": OWNER}, "created_at": T(10, "10:30")}
        prs = [pr(1, T(10, "10:00"), T(10, "11:00")), pr(2, T(10, "10:00"), T(10, "11:00"))]
        r = self.rows(self.build(world(prs, {1: [late], 2: [early]}), now=self.LATE))
        self.assertEqual((r[1]["north"], r[2]["north"]), ("autonomous", "owner_action"))

    def test_a_cohort_is_not_reported_until_its_48h_horizon_closes(self):
        prs = [pr(1, T(12, "10:00"), T(12, "11:00"))]
        early = self.build(world(prs), now=dt.datetime(2026, 11, 14, 12, tzinfo=UTC))  # horizon closes 11-15 00:00
        self.assertFalse(early["repos"][REPO]["per_day"]["2026-11-12"]["closed"])
        self.assertEqual(early["window"]["per_repo"][REPO]["ready_prs"], 0)
        self.assertEqual(early["window"]["per_repo"][REPO]["open_days_not_reported"], ["2026-11-12"])
        self.assertIn("horizon open: not reported", pm.markdown(early))
        done = self.build(world(prs), now=dt.datetime(2026, 11, 15, 1, tzinfo=UTC))
        self.assertEqual(done["window"]["per_repo"][REPO]["ready_prs"], 1)

    def test_still_draft_pr_is_never_ready_and_leaves_the_cohort(self):
        out = self.build(world([unmerged(1, T(10, "10:00"), draft=True)]), now=self.LATE)
        self.assertEqual(out["repos"][REPO]["excluded"], {"never_ready": 1})

    def test_unknown_above_20_percent_prints_a_range_and_small_n_is_insufficient(self):
        cm = {1: [commit("c1", OWNER, "x")]}
        prs = [pr(1, T(10, "10:00"), T(10, "11:00")), pr(2, T(10, "10:00"), T(10, "11:00"))]
        out = self.build(world(prs, commits=cm), now=self.LATE)
        ns = out["repos"][REPO]["per_day"]["2026-11-10"]["north_star"]
        self.assertTrue(ns["unknown_over_20pct"] and ns["insufficient_sample"])
        self.assertEqual(ns["rate_range_pct"], [50.0, 100.0])
        md = pm.markdown(out)
        self.assertIn("insufficient sample (n<50)", md)
        self.assertIn("50.0-100.0%", md)


class Proxies(Base):
    LATE = dt.datetime(2026, 11, 20, tzinfo=UTC)

    def test_pushes_split_into_repair_author_and_merge_main(self):
        bot = {"login": "x[bot]"}
        tl = {1: [{"event": "commented", "actor": bot, "body": "HOLD: red", "created_at": T(10, "10:30")}]}
        cm = {1: [commit("a", date=T(10, "10:10")), commit("b", date=T(10, "10:40")),
                  commit("c", msg="Merge branch 'main' into feat", date=T(10, "10:50")), commit("old", date=T(10, "09:00"))]}
        out = self.build(world([pr(1, T(10, "10:00"), T(10, "11:00"))], tl, cm), now=self.LATE)
        self.assertEqual(self.rows(out)[1]["pushes"], {"repair": 1, "author": 1, "merge_main": 1})

    def test_queue_attempts_and_ejections_by_cause_exclude_the_merged_removal(self):
        g = {"actor": {"login": "github-merge-queue[bot]"}}
        tl = {1: [dict(g, event="added_to_merge_queue", created_at=T(10, "10:20")),
                  dict(g, event="removed_from_merge_queue", reason="failed_checks", created_at=T(10, "10:30")),
                  dict(g, event="added_to_merge_queue", created_at=T(10, "10:40")),
                  dict(g, event="removed_from_merge_queue", reason="merged", created_at=T(10, "10:55"))]}
        mq = self.build(world([pr(1, T(10, "10:00"), T(10, "11:00"))], tl), now=self.LATE)["repos"][REPO]["per_day"]["2026-11-10"]["merge_queue"]
        self.assertEqual((mq["attempts_per_ready_pr"], mq["ejections_by_cause"]), (2.0, {"failed_checks": 1}))

    def test_dirty_per_100_prs_uses_the_ticks(self):
        prs = [pr(1, T(10, "10:00"), T(10, "11:00")), pr(2, T(10, "10:00"), T(10, "11:00"))]
        out = pm.build([REPO], self.cache, 200, OWNER, START, self.LATE, {}, {}, None, world(prs), None, {REPO: {2}})
        self.assertEqual(out["repos"][REPO]["per_day"]["2026-11-10"]["dirty_per_100_prs"], 50.0)

    def test_a_push_on_a_merge_group_sha_counts_as_a_repeat_run(self):
        p = pr(1, T(10, "09:00"), T(10, "11:00"))
        runs = [{"id": 21, "head_sha": "grp", "event": "merge_group", "status": "completed", "name": "GPU", "head_branch": "gh-readonly-queue/main/pr-1-x"},
                {"id": 22, "head_sha": "grp", "event": "push", "status": "completed", "name": "GPU", "head_branch": "main"},
                {"id": 23, "head_sha": "zzz", "event": "push", "status": "completed", "name": "GPU", "head_branch": "main"}]
        job = [{"labels": ["self-hosted"], "started_at": T(10, "10:00"), "completed_at": T(10, "10:10"), "conclusion": "success"}]
        out = self.build(world([p], runs=runs, jobs={21: job, 22: job}), now=self.LATE)
        c = self.rows(out)[1]["compute"]
        self.assertEqual((c["runs"], c["repeat_runs"], c["group_runs"]), (2, 1, 1))


class VellumControl(unittest.TestCase):
    def test_the_vellum_push_runs_on_a_merge_group_sha_count_as_repeats(self):
        runs = json.loads((HERE / "fixtures" / "vellum-gpu-runs-20261113-15.json").read_text())
        self.assertEqual(pm.push_repeats_of_group_shas(runs), (44, 42))


class Controls(Base):
    LATE = dt.datetime(2026, 11, 30, tzinfo=UTC)

    def run_controls(self, with_revert):
        p119 = pr(119, T(5, "09:00"), T(5, "10:00"), sha="s119" + "0" * 36)
        p116 = pr(116, T(5, "11:00"), T(5, "12:00"), sha="s116" + "0" * 36)
        p126 = pr(126, T(6, "09:00"), T(6, "10:00"))
        p126["title"] = "fix flake from #116"
        bc = [commit(p119["merge_commit_sha"], date=T(5, "10:00")), commit(p116["merge_commit_sha"], date=T(5, "12:00"))]
        if with_revert:
            bc.append(commit("rev", msg='Revert "x (#119)"', date=T(6, "08:00")))
        ok = [{"name": "Test", "status": "completed", "conclusion": "success"}]
        gh = world([p119, p116, p126], base_commits=bc, checks={p119["merge_commit_sha"]: ok, p116["merge_commit_sha"]: ok, "rev": ok})
        ctl = {REPO: {"revert": [(119, 124)], "escape": [(116, 126)]}}
        return pm.build([REPO], self.cache, 300, OWNER, START, self.LATE, {}, {}, None, gh, None, None, ctl)

    def test_controls_found_report_normally_with_their_counts_beside_zeros(self):
        out = self.run_controls(True)
        c = out["repos"][REPO]["controls"]
        self.assertEqual((c["status"], c["revert"]["found"], c["escape"]["found"]), ("ok", 1, 1))
        self.assertFalse(out["controls_failed"])
        self.assertIn("reverts 1/1, escapes 1/1", pm.markdown(out))

    def test_a_missed_control_withholds_only_its_metric(self):
        out = self.run_controls(False)
        c = out["repos"][REPO]["controls"]
        self.assertEqual((c["status"], c["disabled"]), ("failed", ["correctness"]))
        md = pm.markdown(out)
        self.assertIn("CONTROL FAILED for some metrics", md)
        self.assertIn("| ready day |", md)  # the report is still printed
        self.assertIn("withheld", md)

    def test_the_pulp_escape_controls_withhold_escapes_the_detector_cannot_find(self):
        # The real fixes do not name the PR they fix, so the built-in controls must fail
        # rather than let Pulp report zero escaped defects.
        pulp = "Generous-Corp/pulp"
        p12219 = pr(12219, T(13, "09:00"), T(14, "06:30"))
        p12338 = pr(12338, T(15, "09:00"), T(15, "16:42"))
        p12375 = pr(12375, T(15, "20:00"), T(16, "01:14"))
        p12375["title"] = "fix(ci): review the cheap-first jobs in the pr_number dispatch allowlist"
        p12384 = pr(12384, T(15, "22:00"), T(16, "04:16"))
        p12384["title"] = "fix(web): link spectral_graph_persistence into the sample-region WASM closure"
        prs = [p12219, p12338, p12375, p12384]
        bc = [commit(p["merge_commit_sha"], date=p["merged_at"]) for p in prs]
        ok = [{"name": "Test", "status": "completed", "conclusion": "success"}]
        gh = world(prs, base_commits=bc, checks={p["merge_commit_sha"]: ok for p in prs})
        out = pm.build([pulp], self.cache, 300, OWNER, START, self.LATE, {}, {}, None, gh)
        c = out["repos"][pulp]["controls"]
        self.assertEqual((c["status"], c["escape"]["expected"], c["escape"]["found"], c["disabled"]),
                         ("failed", 2, 0, ["escaped"]))
        self.assertTrue(out["controls_failed"])

    def test_refused_prs_are_context_and_never_fail_the_control(self):
        prs = [pr(1, T(10, "10:00"), T(10, "11:00"))]
        out = pm.build([REPO], self.cache, 200, OWNER, START, self.LATE, {}, {}, None, world(prs), None, None, {}, {REPO: {1}})
        c = out["repos"][REPO]["controls"]
        self.assertEqual(c["owner_touched"], {"in_report": 1, "non_autonomous": 0, "not_in_window": 0})
        self.assertNotEqual(c["status"], "failed")
        self.assertFalse(out["controls_failed"])

    def test_a_pinned_owner_touched_pr_read_as_autonomous_withholds_the_north_star_only(self):
        prs = [pr(1, T(10, "10:00"), T(10, "11:00"))]
        out = pm.build([REPO], self.cache, 200, OWNER, START, self.LATE, {}, {}, None, world(prs), None, None, {REPO: {"owner": [1]}})
        c = out["repos"][REPO]["controls"]
        self.assertEqual((c["status"], c["disabled"]), ("failed", ["north_star"]))
        md = pm.markdown(out)
        self.assertIn("| ready day |", md)
        self.assertIn("withheld", md)

    def test_a_pinned_owner_touched_pr_found_passes(self):
        owner_pr = {1: [{"event": "merged", "actor": {"login": OWNER}, "created_at": T(10, "10:50")}]}
        prs = [pr(1, T(10, "10:00"), T(10, "11:00"))]
        out = pm.build([REPO], self.cache, 200, OWNER, START, self.LATE, {}, {}, None, world(prs, owner_pr), None, None, {REPO: {"owner": [1]}})
        self.assertEqual(out["repos"][REPO]["controls"]["status"], "ok")
        self.assertNotIn("withheld", pm.markdown(out))


class Truncation(Base):
    LATE = dt.datetime(2026, 11, 30, tzinfo=UTC)

    def test_a_repo_with_unread_prs_is_withheld_and_a_fully_read_repo_is_not(self):
        prs = [pr(n, T(10, "10:00"), T(10, "11:00")) for n in range(1, 4)]
        out = pm.build([REPO], self.cache, 4, OWNER, START, self.LATE, {}, {}, None, world(prs), None, None, {})
        v = out["repos"][REPO]
        self.assertGreater(v["unread_prs"], 0)
        self.assertTrue(out["truncated"])
        self.assertEqual((v["per_day"], v["prs"], out["window"]["per_repo"]), ({}, [], {}))  # no number for a repo with unread PRs
        self.assertEqual(v["withheld"]["total_prs"], 3)
        md = pm.markdown(out)
        self.assertIn(f"**WITHHELD: {v['unread_prs']} of 3 PRs unread", md)
        self.assertNotIn("| ready day |", md)
        with tempfile.TemporaryDirectory() as d2:
            full = pm.build([REPO], d2, 300, OWNER, START, self.LATE, {}, {}, None, world(prs), None, None, {})
        self.assertEqual(full["repos"][REPO]["unread_prs"], 0)
        self.assertNotIn("withheld", full["repos"][REPO])
        self.assertNotIn("WITHHELD", pm.markdown(full))

    def test_a_cut_short_listing_withholds_the_repo_too(self):
        gh = world([pr(1, T(10, "10:00"), T(10, "11:00"))])
        full_page = [pr(n, T(10, "10:00"), T(10, "11:00")) for n in range(1, 101)]
        gh.routes[0] = (r"pulls\?state=all", full_page)  # eight full pages that never reach the window's start
        for n in range(2, 101):
            gh.routes.append((rf"issues/{n}/timeline", []))
            gh.routes.append((rf"pulls/{n}/commits", [commit(f"c{n}")]))
        out = pm.build([REPO], self.cache, 5000, OWNER, dt.datetime(2026, 11, 1, tzinfo=UTC), self.LATE, {}, {}, None, gh, None, None, {})
        self.assertFalse(out["repos"][REPO]["listing_complete"])
        self.assertIn("withheld", out["repos"][REPO])
        self.assertEqual(out["repos"][REPO]["prs"], [])

    def test_the_oldest_cohort_day_is_read_first_so_a_cap_leaves_the_newest_unread(self):
        old, mid, new = (pr(1, T(8, "10:00"), T(8, "11:00")), pr(2, T(9, "10:00"), T(9, "11:00")), pr(3, T(10, "10:00"), T(10, "11:00")))
        gh = world([new, mid, old])  # the API lists newest first
        pm.build([REPO], self.cache, 5, OWNER, START, self.LATE, {}, {}, None, gh, None, None, {})  # the listing plus two PRs
        facts = sorted(os.listdir(os.path.join(self.cache, REPO.replace("/", "__"), "facts")))
        self.assertEqual(facts, ["1.json", "2.json"])
        self.assertEqual([c for c in gh.calls if "timeline" in c][:1], [c for c in gh.calls if "issues/1/timeline" in c][:1])

    def test_truncated_with_all_facts_cached_is_still_partial(self):
        v = {"truncated": True, "unread_prs": 0, "listing_complete": True, "read_errors": 0}
        self.assertTrue(pm.repo_partial(v))
        self.assertTrue(pm.repo_partial(dict(v, truncated=False, read_errors=1)))
        self.assertFalse(pm.repo_partial(dict(v, truncated=False)))

    def test_pinned_owner_control_is_rendered(self):
        prs = [pr(1, T(10, "10:00"), T(10, "11:00"))]
        out = pm.build([REPO], self.cache, 200, OWNER, START, self.LATE, {}, {}, None, world(prs), None, None, {REPO: {"owner": [1]}})
        self.assertIn("owner-pinned 0/1 (failed)", pm.markdown(out))

    def test_settled_prs_are_cached_between_runs(self):
        prs = [pr(n, T(10, "10:00"), T(10, "11:00")) for n in range(1, 4)]
        pm.build([REPO], self.cache, 300, OWNER, START, self.LATE, {}, {}, None, world(prs), None, None, {})
        again = pm.build([REPO], self.cache, 300, OWNER, START, self.LATE, {}, {}, None, world(prs), None, None, {})
        self.assertTrue(os.path.isdir(os.path.join(self.cache, REPO.replace("/", "__"), "facts")))
        self.assertLess(again["repos"][REPO]["reads"], 10)


class Intervals(unittest.TestCase):
    def test_wilson_interval(self):
        self.assertIsNone(pm.wilson(0, 0))
        lo, hi = pm.wilson(8, 10)
        self.assertAlmostEqual(lo, 49.0, delta=0.2)
        self.assertAlmostEqual(hi, 94.3, delta=0.2)
        self.assertEqual(pm.wilson(0, 5)[0], 0.0)


class WindowVsBaseline(Base):
    def test_window_and_baseline_print_side_by_side_with_n_and_interval(self):
        prs = [pr(1, T(12, "10:00"), T(12, "11:00")), pr(2, T(13, "10:00"), T(13, "11:00")), pr(3, T(14, "08:00"), T(14, "09:00"))]
        b = (dt.datetime(2026, 11, 12, tzinfo=UTC), dt.datetime(2026, 11, 13, tzinfo=UTC))
        out = pm.build([REPO], self.cache, 200, OWNER, dt.datetime(2026, 11, 14, tzinfo=UTC), dt.datetime(2026, 11, 20, tzinfo=UTC), {}, {}, b, world(prs))
        self.assertEqual(out["baseline"]["per_repo"][REPO]["ready_prs"], 2)
        self.assertEqual(out["window"]["per_repo"][REPO]["ready_prs"], 1)
        md = pm.markdown(out)
        self.assertIn("## Window 2026-11-14", md)
        self.assertIn("## Baseline 2026-11-12 to 2026-11-13", md)
        self.assertIn("CI95", md)
        self.assertIn("Scorecard " + pm.SCORECARD_VERSION, md)
        self.assertIn("Baseline noise floor", md)
        self.assertIn("truncated=false", md)


class Caps(Base):
    def test_read_cap_truncates_and_flags_it_and_the_next_run_resumes_from_cache(self):
        prs = [pr(i, T(10, "10:00"), T(10, "11:00")) for i in (1, 2, 3, 4)]
        out = self.build(world(prs), cap=6)  # the listing (1) plus 2 reads per PR: two PRs fit
        v = out["repos"][REPO]
        self.assertTrue(out["truncated"] and v["truncated"])
        self.assertGreater(v["unread_prs"], 0)
        self.assertEqual(out["coverage"]["status"], "partial")
        md = pm.markdown(out)
        self.assertIn(f"**WITHHELD: {v['unread_prs']} of 4 PRs unread", md)
        again = self.build(world(prs), cap=200)
        self.assertNotIn("WITHHELD", pm.markdown(again))
        self.assertFalse(again["truncated"])
        self.assertEqual(len(again["repos"][REPO]["prs"]), 4)

    def test_baseline_pools_the_named_days_and_reaches_back_before_the_window(self):
        prs = [pr(1, T(12, "10:00"), T(12, "11:00")), pr(2, T(13, "10:00"), T(13, "12:00")), pr(3, T(9, "10:00"), T(9, "11:00"))]
        b = (dt.datetime(2026, 11, 12, tzinfo=UTC), dt.datetime(2026, 11, 14, tzinfo=UTC))
        out = pm.build([REPO], self.cache, 200, OWNER, dt.datetime(2026, 11, 13, tzinfo=UTC), dt.datetime(2026, 11, 20, tzinfo=UTC), {}, {}, b, world(prs))
        self.assertEqual(out["baseline"]["per_repo"][REPO]["ready_prs"], 2)
        self.assertEqual(out["baseline"]["per_repo"][REPO]["handoff_min"]["median"], 90.0)
        self.assertIn("## Baseline 2026-11-12 to 2026-11-14", pm.markdown(out))


class Integrated(unittest.TestCase):
    def test_build_report_adds_the_section_and_markdown_without_e2e_says_unavailable(self):
        with tempfile.TemporaryDirectory() as d, tempfile.TemporaryDirectory() as c:
            t = dt.datetime(2026, 11, 11, 9, 0, tzinfo=UTC)
            tick = {"repos": [{"repo": REPO, "prs": [], "errors": [], "required_contexts": ["Test (app_id=1)"],
                               "base_poison": {"red_everywhere": {"Test": "2026-11-11T09:30:00Z"}}}]}
            (Path(d) / f"acme_repo-{lr.dt.datetime.strftime(t, '%Y%m%dT%H%M%SZ')}.json").write_text(json.dumps(tick))
            p = pr(1, T(11, "08:00"), T(11, "09:00"))
            gh = world([p], base_commits=[commit(p["merge_commit_sha"], date=T(11, "09:00"))])
            rep = lr.build_report(d, dt.timedelta(days=4), NOW, e2e={"cache_dir": c, "cap": 50, "owner": OWNER, "fetch": gh, "repos": [REPO]})
            e = rep["end_to_end"]["repos"][REPO]
            self.assertEqual(e["per_day"]["2026-11-11"]["ready_prs"], 1)
            self.assertEqual(e["prs"][0]["correct"]["base_red_source"], "ticks")
            self.assertIn("end-to-end per eligible PR", lr.markdown(rep))
            self.assertFalse([x for x in gh.calls if "rules/branches" in x])  # required contexts came from the tick
            plain = lr.build_report(d, dt.timedelta(days=4), NOW)
            self.assertIsNone(plain["end_to_end"])
            self.assertIn("no --e2e-cache", lr.markdown(plain))

    def test_defaults_are_the_five_fleet_repos_and_a_1500_cap(self):
        seen = {}

        def fake_build(repos, cache, cap, *a, **k):
            seen.update(repos=list(repos), cap=cap)
            return {}
        with tempfile.TemporaryDirectory() as d, tempfile.TemporaryDirectory() as c:
            orig = pm.build
            pm.build = fake_build
            try:
                lr.build_report(d, dt.timedelta(days=1), NOW, e2e={"cache_dir": c})
            finally:
                pm.build = orig
        self.assertEqual(seen["cap"], 1500)
        self.assertEqual(seen["repos"], ["Generous-Corp/pulp", "Generous-Corp/vellum", "danielraffel/Shipyard",
                                         "danielraffel/tartci", "danielraffel/fleet-handbook"])


class SyntheticControl(unittest.TestCase):
    def broken(self, name, fn):
        """Run the control with one scorecard function replaced by a broken one."""
        orig = getattr(pm, name)
        setattr(pm, name, fn)
        try:
            return sc.run(pm)
        finally:
            setattr(pm, name, orig)

    def test_the_control_passes_on_the_real_scorecard_and_covers_every_metric_family(self):
        r = sc.run(pm)
        self.assertEqual((r["status"], r["failed"], r["max_abs_delta"]), ("pass", [], 0.0))
        families = {c["name"].split(".")[0] for c in r["checks"]}
        self.assertTrue({"north_star", "handoff_min", "rounds", "pushes_total", "merge_queue", "interventions",
                         "compute_per_merged_pr", "correctness", "attribution"} <= families)
        self.assertEqual(sc.line(r).split(":")[0], "CONTROL PASS")

    def test_expected_values_are_computed_without_the_scorecard(self):
        import inspect
        for fn in (sc.expected, sc.raw_world):
            self.assertNotIn("pr_metrics", inspect.getsource(fn))
        e = sc.expected()
        self.assertEqual((e["north_star.autonomous"], e["north_star.owner_action"], e["north_star.unknown"], e["north_star.not_merged_in_48h"]), (10, 2, 2, 1))
        self.assertEqual((e["handoff_min.median"], e["handoff_min.p90"], e["compute_per_merged_pr.repeat_runs_total"]), (70.0, 120, 2))  # sorted delays 20 25 30 35 45 55 65 75 85 95 100 110 120 150: median (65+75)/2, 13th of 14

    def test_the_control_reads_nothing_from_github_and_unknown_paths_fail_loudly(self):
        with self.assertRaises(KeyError):
            sc.raw_world()("repos/other/repo/pulls?state=all")

    def test_a_broken_north_star_fails_the_control_with_deltas(self):
        r = self.broken("north_star", lambda f, commits=True, attributed=True: "autonomous")
        self.assertEqual(r["status"], "fail")
        self.assertIn("north_star.owner_action", r["failed"])
        self.assertIn("expected 2 got 0 (delta -2.0)", sc.line(r))

    def test_each_family_of_numbers_is_guarded(self):
        cases = {
            "pctile": (lambda xs, p: 0.0, "handoff_min.p90"),
            "revert_of": (lambda commits, f: None, "correctness.reverted"),
            "escaped_by": (lambda later, f: None, "correctness.escaped"),
            "commit_red": (lambda rd, sha, required: (set(), True), "correctness.base_red"),
            "push_repeats_of_group_shas": (lambda runs: (0, 0), None),
            "steward_evidence": (lambda rd, f: False, "attribution.steward_merges_recognised"),
            "path_of": (lambda f, comp: ("clean", []), "paths.slow.n"),
            "cause_minutes": (lambda f, comp: {"hold": 0.0, "red": 0.0, "queue": 0.0, "runner": 0.0, "dirty": None}, "paths.cause_min.red.mean_min"),
        }
        for name, (fn, expect) in cases.items():
            if expect is None:
                continue
            r = self.broken(name, fn)
            self.assertEqual(r["status"], "fail", name)
            self.assertIn(expect, r["failed"], name)

    def test_a_crash_in_the_scorecard_fails_the_control(self):
        def boom(*a, **k):
            raise RuntimeError("instrument broke")
        r = self.broken("pr_facts", boom)
        self.assertEqual(r["status"], "fail")
        self.assertIn("RuntimeError", sc.line(r))

    def test_the_report_prints_control_pass_with_the_numbers(self):
        out = pm.build([REPO], tempfile.mkdtemp(), 50, OWNER, START, NOW, {}, {}, None, world([pr(1, T(10, "10:00"), T(10, "11:00"))]))
        self.assertEqual(out["synthetic_control"]["status"], "pass")
        md = pm.markdown(out)
        self.assertIn("**CONTROL PASS: 54 of 54", md)
        self.assertIn("| ready day |", md)

    def test_a_repo_with_no_live_control_reads_synthetic_not_unchecked(self):
        gh = world([pr(1, T(10, "10:00"), T(10, "11:00"))])
        out = pm.build([REPO], tempfile.mkdtemp(), 50, OWNER, START, NOW, {}, {}, None, gh)
        self.assertEqual(out["repos"][REPO]["controls"]["status"], "synthetic")
        self.assertIn("Controls (synthetic)", pm.markdown(out))
        off = pm.build([REPO], tempfile.mkdtemp(), 50, OWNER, START, NOW, {}, {}, None, world([pr(1, T(10, "10:00"), T(10, "11:00"))]), control=False)
        self.assertEqual(off["repos"][REPO]["controls"]["status"], "unchecked")

    def test_a_failed_control_withholds_every_outcome_number(self):
        failing = lambda: sc.run(pm) | {"status": "fail", "failed": ["north_star.autonomous"]}
        out = pm.build([REPO], tempfile.mkdtemp(), 50, OWNER, START, NOW, {}, {}, None, world([pr(1, T(10, "10:00"), T(10, "11:00"))]),
                       control=failing)
        self.assertIn("synthetic control failed", out["withheld"])
        self.assertEqual((out["repos"][REPO]["per_day"], out["repos"][REPO]["prs"], out["window"]["per_repo"], out["baseline"]), ({}, [], {}, None))
        md = pm.markdown(out)
        self.assertIn("CONTROL FAIL", md)
        self.assertIn("withheld", md)
        for number_cells in ("| ready day |", "rate (CI95)", "autonomous a/o/unknown", "handoff median"):
            self.assertNotIn(number_cells, md)


class Acceptance(unittest.TestCase):
    def summaries(self, **over):
        def s(med, pr_lvl=0.0, mg=0.0, mac=0.0, q2m=0.0):
            return {"handoff_min": {"median": med}, "merge_queue": {"enqueue_to_merge_min": {"median": q2m}},
                    "compute_per_merged_pr": {"macos_min_mean": mac, "by_workflow": {acc.PROOF: {"pull_request": pr_lvl, "merge_group": mg}}}}
        base = {"Generous-Corp/vellum": s(365.0, 2.9, 0.87, 0.0, 54.0), "Generous-Corp/pulp": s(92.0, mac=69.0)}
        base.update(over)
        return base

    def test_the_hand_measured_values_reproduce_exactly_and_all_six_are_within(self):
        rows = acc.compare(self.summaries())
        self.assertEqual(len(rows), 6)
        self.assertTrue(all(r["within"] and r["delta_pct"] == 0.0 for r in rows))

    def test_a_number_outside_the_tolerance_or_missing_is_not_within(self):
        v = self.summaries()["Generous-Corp/vellum"]
        v["handoff_min"]["median"] = 365.0 * 1.2
        del v["compute_per_merged_pr"]["by_workflow"][acc.PROOF]
        rows = {r["name"]: r for r in acc.compare(self.summaries(**{"Generous-Corp/vellum": v}))}
        self.assertEqual((rows["Vellum ready-to-merge median"]["within"], rows["Vellum ready-to-merge median"]["delta_pct"]), (False, 20.0))
        self.assertEqual((rows["Vellum PR GPU proofs per merged PR"]["report"], rows["Vellum PR GPU proofs per merged PR"]["within"]), (None, False))
        self.assertTrue(rows["Pulp ready-to-merge median"]["within"])
        self.assertIn("| Vellum ready-to-merge median | 365.0 min | 438.0 | 20.0% | NO |", acc.table(list(rows.values()), 0.15))

    def test_the_tolerance_edge(self):
        v = self.summaries()["Generous-Corp/vellum"]
        v["handoff_min"]["median"] = 365.0 * 1.15
        self.assertTrue({r["name"]: r for r in acc.compare(self.summaries(**{"Generous-Corp/vellum": v}))}["Vellum ready-to-merge median"]["within"])
        v["handoff_min"]["median"] = 365.0 * 1.16
        self.assertFalse({r["name"]: r for r in acc.compare(self.summaries(**{"Generous-Corp/vellum": v}))}["Vellum ready-to-merge median"]["within"])


if __name__ == "__main__":
    unittest.main()

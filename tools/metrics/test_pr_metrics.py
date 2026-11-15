import datetime as dt
import json
import re
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import pr_metrics as pm  # noqa: E402
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
            "merge_commit_sha": sha or f"m{n:040d}"[:40], "base": {"ref": "main"}, "head": {"sha": f"h{n:040d}"[:40]}}


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


def world(prs, timelines=None, commits=None, base_commits=None, runs=None, jobs=None, checks=None, required=("Test",)):
    timelines, commits, jobs, checks = timelines or {}, commits or {}, jobs or {}, checks or {}
    routes = [(r"pulls\?state=closed", prs)]
    for n in {p["number"] for p in prs}:
        routes.append((rf"issues/{n}/timeline", timelines.get(n, [])))
        routes.append((rf"pulls/{n}/commits", commits.get(n, [commit(f"c{n}")])))
    routes.append((r"/commits\?sha=main", base_commits or []))
    routes.append((r"actions/runs\?created", {"workflow_runs": runs or []}))
    for rid, js in jobs.items():
        routes.append((rf"actions/runs/{rid}/jobs", {"jobs": js}))
    for sha, cr in checks.items():
        routes.append((rf"commits/{sha}/check-runs", {"check_runs": cr}))
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


class Autonomy(Base):
    def test_owner_label_is_an_owner_action_and_bot_events_are_not(self):
        tl = {1: [{"event": "labeled", "actor": {"login": "shipyard-local[bot]"}, "created_at": T(10, "10:30")}],
              2: [{"event": "labeled", "actor": {"login": OWNER}, "created_at": T(10, "10:30")}]}
        out = self.build(world([pr(1, T(10, "10:00"), T(10, "11:00")), pr(2, T(10, "10:00"), T(10, "11:00"))], tl))
        r = self.rows(out)
        self.assertEqual((r[1]["autonomy"], r[2]["autonomy"]), ("autonomous", "owner_action"))
        a = out["repos"][REPO]["per_day"]["2026-11-10"]["autonomous"]
        self.assertEqual((a["autonomous"], a["owner_action"], a["unknown"], a["rate_pct"]), (1, 1, 0, 50.0))

    def test_owner_commit_without_agent_trailer_is_unknown_with_one_is_autonomous(self):
        cm = {1: [commit("c1", OWNER, "fix it")],
              2: [commit("c2", OWNER, "fix it\n\nCo-Authored-By: Claude Sonnet <noreply@anthropic.com>")]}
        out = self.build(world([pr(1, T(10, "10:00"), T(10, "11:00")), pr(2, T(10, "10:00"), T(10, "11:00"))], commits=cm))
        r = self.rows(out)
        self.assertEqual((r[1]["autonomy"], r[2]["autonomy"]), ("unknown", "autonomous"))
        a = out["repos"][REPO]["per_day"]["2026-11-10"]["autonomous"]
        self.assertEqual((a["unknown"], a["rate_pct"]), (1, 100.0))  # rate is over determinable PRs only

    def test_version_bump_trailer_marks_agent_work_and_github_only_ignores_commits(self):
        cm = {1: [commit("c1", OWNER, "chore\n\nVersion-Bump: cli=skip reason=x")], 2: [commit("c2", OWNER, "by hand")]}
        out = self.build(world([pr(1, T(10, "10:00"), T(10, "11:00")), pr(2, T(10, "10:00"), T(10, "11:00"))], commits=cm))
        r = self.rows(out)
        self.assertEqual((r[1]["autonomy"], r[2]["autonomy"], r[2]["autonomy_github_only"]), ("autonomous", "unknown", "autonomous"))
        a = out["repos"][REPO]["per_day"]["2026-11-10"]["autonomous"]
        self.assertEqual((a["unknown_share_pct"], a["github_only"]["unknown"], a["github_only"]["rate_pct"]), (50.0, 0, 100.0))

    def test_a_definite_owner_action_beats_an_ambiguous_commit(self):
        tl = {1: [{"event": "commented", "user": {"login": OWNER}, "created_at": T(10, "10:30")}]}
        out = self.build(world([pr(1, T(10, "10:00"), T(10, "11:00"))], tl, {1: [commit("c1", OWNER)]}))
        self.assertEqual(self.rows(out)[1]["autonomy"], "owner_action")

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

    def test_an_unreadable_jobs_list_is_unread_not_zero(self):
        gh = world([pr(1, T(10, "09:00"), T(10, "11:00"))], runs=self.runs, jobs={})
        out = self.build(gh)
        self.assertIsNone(self.rows(out)[1]["compute"])
        self.assertEqual(out["repos"][REPO]["per_day"]["2026-11-10"]["compute_per_pr"]["prs_read"], 0)


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
        self.assertTrue(k["provisional"])
        self.assertEqual(k["provisional_prs"], 1)
        self.assertIn("provisional", pm.markdown(out))

    def test_unreadable_checks_make_correctness_unknown(self):
        p = self.merged_old(6)
        out = self.build(world([p], base_commits=[commit(p["merge_commit_sha"], date=T(5, "10:00"))]), now=dt.datetime(2026, 11, 14, tzinfo=UTC))
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
        self.assertEqual((r["day"], r["merge_day"]), ("2026-11-10", "2026-11-11"))
        self.assertIn("2026-11-10", out["repos"][REPO]["per_day"])

    def test_owner_actions_are_counted_apart_from_repair_cycles_and_holds(self):
        bot = {"login": "shipyard-local[bot]"}
        tl = {1: [{"event": "commented", "actor": bot, "body": "HOLD: red", "created_at": T(10, "10:10")},
                  {"event": "commented", "actor": bot, "body": "repaired: fix at abc", "created_at": T(10, "10:20")},
                  {"event": "commented", "actor": bot, "body": "repaired: again", "created_at": T(10, "10:30")},
                  {"event": "labeled", "actor": {"login": OWNER}, "created_at": T(10, "10:40")},
                  {"event": "commented", "actor": {"login": OWNER}, "body": "ok", "created_at": T(10, "10:50")}]}
        out = self.build(world([pr(1, T(10, "10:00"), T(10, "11:00"))], tl))
        r = self.rows(out)[1]
        self.assertEqual((r["owner_actions"], r["repair_cycles"], r["holds"]), (2, 2, 1))
        iv = out["repos"][REPO]["per_day"]["2026-11-10"]["interventions"]
        self.assertEqual((iv["owner_actions_per_pr"], iv["repair_cycles_per_pr"], iv["prs_with_repair_cycle"]), (2.0, 2.0, 1))


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
        out = pm.build([REPO], self.cache, 200, OWNER, dt.datetime(2026, 11, 14, tzinfo=UTC), NOW, {}, {}, b, world(prs))
        self.assertEqual(out["baseline"]["per_repo"][REPO]["merged"], 2)
        self.assertEqual(out["window"]["per_repo"][REPO]["merged"], 1)
        md = pm.markdown(out)
        self.assertIn("## Window 2026-11-14", md)
        self.assertIn("## Baseline 2026-11-12 to 2026-11-13", md)
        self.assertIn("CI95", md)


class Caps(Base):
    def test_read_cap_truncates_and_flags_it_and_the_next_run_resumes_from_cache(self):
        prs = [pr(i, T(10, "10:00"), T(10, "11:00")) for i in (1, 2, 3, 4)]
        out = self.build(world(prs), cap=6)  # the listing (1) plus 2 reads per PR: two PRs fit
        v = out["repos"][REPO]
        self.assertTrue(out["truncated"] and v["truncated"])
        self.assertGreater(v["unread_prs"], 0)
        self.assertEqual(out["coverage"]["status"], "partial")
        again = self.build(world(prs), cap=200)
        self.assertFalse(again["truncated"])
        self.assertEqual(len(again["repos"][REPO]["prs"]), 4)

    def test_baseline_pools_the_named_days_and_reaches_back_before_the_window(self):
        prs = [pr(1, T(12, "10:00"), T(12, "11:00")), pr(2, T(13, "10:00"), T(13, "12:00")), pr(3, T(9, "10:00"), T(9, "11:00"))]
        b = (dt.datetime(2026, 11, 12, tzinfo=UTC), dt.datetime(2026, 11, 14, tzinfo=UTC))
        out = pm.build([REPO], self.cache, 200, OWNER, dt.datetime(2026, 11, 13, tzinfo=UTC), NOW, {}, {}, b, world(prs))
        self.assertEqual(out["baseline"]["per_repo"][REPO]["merged"], 2)
        self.assertEqual(out["baseline"]["per_repo"][REPO]["handoff_min"]["median"], 90.0)
        self.assertIn("## Baseline 2026-11-12 to 2026-11-14", pm.markdown(out))


class Integrated(unittest.TestCase):
    def test_build_report_adds_the_section_and_markdown_without_e2e_says_unavailable(self):
        with tempfile.TemporaryDirectory() as d, tempfile.TemporaryDirectory() as c:
            t = dt.datetime(2026, 11, 13, 9, 0, tzinfo=UTC)
            tick = {"repos": [{"repo": REPO, "prs": [], "errors": [], "required_contexts": ["Test (app_id=1)"],
                               "base_poison": {"red_everywhere": {"Test": "2026-11-13T09:30:00Z"}}}]}
            (Path(d) / f"acme_repo-{lr.dt.datetime.strftime(t, '%Y%m%dT%H%M%SZ')}.json").write_text(json.dumps(tick))
            p = pr(1, T(13, "08:00"), T(13, "09:00"))
            gh = world([p], base_commits=[commit(p["merge_commit_sha"], date=T(13, "09:00"))])
            rep = lr.build_report(d, dt.timedelta(days=3), NOW, e2e={"cache_dir": c, "cap": 50, "owner": OWNER, "fetch": gh})
            e = rep["end_to_end"]["repos"][REPO]
            self.assertEqual(e["per_day"]["2026-11-13"]["merged"], 1)
            self.assertEqual(e["prs"][0]["correct"]["base_red_source"], "ticks")
            self.assertIn("End-to-end per eligible PR", lr.markdown(rep))
            self.assertFalse([x for x in gh.calls if "rules/branches" in x])  # required contexts came from the tick
            plain = lr.build_report(d, dt.timedelta(days=3), NOW)
            self.assertIsNone(plain["end_to_end"])
            self.assertIn("no --e2e-cache", lr.markdown(plain))


if __name__ == "__main__":
    unittest.main()

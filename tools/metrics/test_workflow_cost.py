import datetime as dt
import json
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import workflow_cost as wc  # noqa: E402

D = dt.date
FLEET = ["self-hosted", "macOS", "ARM64", "acme-build"]
FLEET_MG = ["self-hosted", "macOS", "ARM64", "acme-build-merge-group"]
HOSTED = ["ubuntu-latest"]
WF = ".github/workflows/build.yml"


def run(i, event, sha, conclusion="success", branch="main", name="Build", path=WF, status="completed",
        created="2026-11-12T10:00:00Z"):
    return {"id": i, "name": name, "path": path, "event": event, "status": status, "conclusion": conclusion,
            "head_sha": sha, "head_branch": branch, "created_at": created, "run_attempt": 1}


def job(name, labels, start_min, dur_min, created_min=0, conclusion="success", runner="r1", base="2026-11-12T10:"):
    def t(m):
        return (dt.datetime(2026, 11, 12, 10, 0, tzinfo=dt.timezone.utc) + dt.timedelta(minutes=m)).strftime("%Y-%m-%dT%H:%M:%SZ")
    return {"id": hash(name) % 10**6, "name": name, "status": "completed", "conclusion": conclusion,
            "created_at": t(created_min), "started_at": t(start_min), "completed_at": t(start_min + dur_min),
            "labels": labels, "runner_name": runner, "run_attempt": 1}


def data(runs, jobs, ctx_push=None, ctx_mg=None, partial=()):
    by = {"push": [], "merge_group": [], "pull_request": []}
    for r in runs:
        by[r["event"]].append(r)
    return {"runs": by, "jobs": jobs, "partial": list(partial), "reads": 0, "errors": 0,
            "ctx": {"push": ctx_push if ctx_push is not None else by["push"],
                    "merge_group": ctx_mg if ctx_mg is not None else by["merge_group"]}}


def rep(d, events=("push", "merge_group", "pull_request")):
    return wc.compute(d, "acme/repo", D(2026, 11, 12), D(2026, 11, 12), events, "main")


def row(r, event, workflow="Build"):
    return next(w for w in r["workflows"] if w["event"] == event and w["workflow"] == workflow)


class Cost(unittest.TestCase):
    def test_runner_class_groups_fleet_by_first_repo_label(self):
        self.assertEqual(wc.runner_class(FLEET), ("fleet", "acme-build"))
        self.assertEqual(wc.runner_class(FLEET_MG), ("fleet", "acme-build-merge-group"))
        self.assertEqual(wc.runner_class(["self-hosted", "linux"]), ("fleet", "self-hosted"))
        self.assertEqual(wc.runner_class(["macos-15"]), ("hosted", "macos-15"))

    def test_minutes_split_by_class_and_skipped_or_unassigned_are_zero(self):
        jobs = {1: [job("a", FLEET, 5, 20), job("b", HOSTED, 0, 7), job("c", FLEET, 0, 30, conclusion="skipped"),
                    job("d", FLEET, 0, 40, runner=None)]}
        w = row(rep(data([run(1, "push", "s1")], jobs)), "push")
        self.assertEqual(w["fleet_minutes"], 20.0)
        self.assertEqual(w["hosted_minutes"], 7.0)
        self.assertEqual(w["fleet_by_label"], {"acme-build": 20.0})

    def test_queue_median_p90_and_sum_count_only_jobs_that_got_a_runner(self):
        jobs = {1: [job("a", FLEET, 10, 1, created_min=0), job("b", FLEET, 20, 1, created_min=0),
                    job("c", FLEET, 30, 1, created_min=0), job("n", FLEET, 99, 1, runner=None)]}
        q = row(rep(data([run(1, "pull_request", "s1")], jobs)), "pull_request")["queue_minutes"]
        self.assertEqual((q["n"], q["median"], q["sum"]), (3, 20.0, 60.0))
        self.assertEqual(q["p90"], 28.0)

    def test_duplicates_need_a_successful_merge_group_run_of_the_same_workflow_on_the_same_sha(self):
        runs = [run(1, "push", "s1"), run(2, "push", "s2"), run(3, "push", "s3"),
                run(4, "merge_group", "s1", branch="gh-readonly-queue/main/pr-1-x"),
                run(5, "merge_group", "s2", conclusion="failure", branch="gh-readonly-queue/main/pr-2-x"),
                run(6, "merge_group", "s3", path=".github/workflows/other.yml", branch="gh-readonly-queue/main/pr-3-x")]
        d = rep(data(runs, {r["id"]: [] for r in runs}))["duplicates"]
        self.assertEqual([(x["push_runs"], x["same_sha_merge_group_success"]) for x in d], [(3, 1)])

    def test_push_runs_on_other_branches_are_not_counted_as_duplicates(self):
        runs = [run(1, "push", "s1"), run(2, "push", "s9", branch="feature")]
        d = rep(data(runs, {1: [], 2: []}))["duplicates"]
        self.assertEqual(d[0]["push_runs"], 1)

    def test_short_job_wait_counts_fleet_jobs_under_three_minutes_per_label(self):
        jobs = {1: [job("a", FLEET, 30, 1, created_min=0), job("b", FLEET, 60, 2.5, created_min=0),
                    job("c", FLEET, 10, 10, created_min=0), job("h", HOSTED, 600, 1, created_min=0),
                    job("m", FLEET_MG, 6, 2, created_min=0)]}
        s = {x["label"]: x for x in rep(data([run(1, "pull_request", "s1")], jobs))["short_job_wait"]}
        self.assertEqual((s["acme-build"]["fleet_jobs"], s["acme-build"]["short_jobs"]), (3, 2))
        self.assertEqual(s["acme-build"]["short_queue_hours"], 1.5)
        self.assertEqual(s["acme-build"]["all_queue_hours"], 1.7)
        self.assertEqual(s["acme-build-merge-group"]["short_jobs"], 1)
        self.assertNotIn("ubuntu-latest", s)

    def test_never_landed_is_fleet_minutes_of_merge_groups_whose_sha_was_never_pushed(self):
        mg = "gh-readonly-queue/main/pr-1-x"
        runs = [run(1, "merge_group", "landed", branch=mg), run(2, "merge_group", "lost", branch=mg),
                run(3, "merge_group", "flight", branch=mg, status="in_progress", conclusion=None),
                run(4, "push", "landed")]
        jobs = {1: [job("macos", FLEET, 0, 10), job("l", HOSTED, 0, 99)], 2: [job("macos", FLEET, 0, 25)],
                3: [job("macos", FLEET, 0, 4)], 4: []}
        r = rep(data(runs, jobs))
        n = r["never_landed"][0]
        self.assertEqual((n["runs"], n["in_flight"], n["runs_not_landed"]), (3, 1, 1))
        self.assertEqual((n["fleet_minutes"], n["fleet_minutes_not_landed"]), (35.0, 25.0))  # the in-flight run is excluded
        mac = next(j for j in r["jobs"] if j["job"] == "macos" and j["event"] == "merge_group")
        self.assertEqual((mac["minutes"], mac["minutes_not_landed"]), (39.0, 25.0))

    def test_a_run_whose_jobs_were_not_read_is_partial_not_zero(self):
        runs = [run(1, "push", "s1"), run(2, "push", "s2")]
        r = rep(data(runs, {1: [job("a", FLEET, 0, 10)], 2: None}, partial=["1 of 2 runs' jobs not read in full"]))
        w = row(r, "push")
        self.assertTrue(w["partial"])
        self.assertEqual((w["runs"], w["runs_unread"], w["fleet_minutes"]), (2, 1, 10.0))
        self.assertTrue(r["partial"])
        self.assertIn("PARTIAL", wc.render_md(r))


class Fake:
    """A GitHub that serves runs and jobs from fixtures and counts reads."""

    def __init__(self, listing, jobs, limit=None):
        self.listing, self.jobs, self.limit, self.paths = listing, jobs, limit, []

    def __call__(self, path):
        self.paths.append(path)
        if "/actions/runs?" in path:
            q = dict(kv.split("=", 1) for kv in path.split("?", 1)[1].split("&"))
            lo, hi = q["created"].split("..")
            runs = [r for r in self.listing if r["event"] == q["event"] and lo <= r["created_at"] <= hi
                    and (q["event"] != "push" or r["head_branch"] == q.get("branch"))]
            page, per = int(q["page"]), int(q["per_page"])
            shown = runs[:self.limit] if self.limit else runs  # the search returns at most `limit` results
            return {"total_count": len(runs), "workflow_runs": shown[(page - 1) * per: page * per]}
        rid = int(path.split("/runs/")[1].split("/")[0])
        return {"total_count": len(self.jobs[rid]), "jobs": self.jobs[rid]}


class Collect(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        mg = "gh-readonly-queue/main/pr-1-x"
        self.listing = [run(1, "push", "s1"), run(2, "merge_group", "s1", branch=mg)]
        self.jobs = {1: [job("a", HOSTED, 0, 3)], 2: [job("macos", FLEET, 0, 10)]}

    def go(self, fake, cap=1000, events=("push", "merge_group"), now=None):
        out = Path(self.tmp) / "o.json"
        wc.main(["--repo", "acme/repo", "--since", "2026-11-12", "--until", "2026-11-12", "--events", ",".join(events),
                 "--cache", self.tmp, "--read-cap", str(cap), "--json-out", str(out), "--workers", "1", "--quiet"],
                now=now or dt.datetime(2026, 11, 20, tzinfo=dt.timezone.utc), fetch=fake)
        return json.loads(out.read_text())

    def test_end_to_end_reads_each_run_once_and_a_second_run_reads_nothing_for_completed_days(self):
        f1 = Fake(self.listing, self.jobs)
        r = self.go(f1)
        self.assertFalse(r["partial"])
        self.assertEqual(row(r, "push")["hosted_minutes"], 3.0)
        self.assertEqual(r["never_landed"][0]["fleet_minutes_not_landed"], 0.0)
        self.assertEqual(r["duplicates"][0]["same_sha_merge_group_success"], 1)
        f2 = Fake(self.listing, self.jobs)
        r2 = self.go(f2)
        self.assertEqual(f2.paths, [])
        self.assertEqual(r2["workflows"], r["workflows"])

    def test_a_day_over_the_search_cap_is_split_by_time_and_read_in_full(self):
        times = ["02:00", "05:00", "09:00", "13:00", "17:00", "22:00"]
        listing = [run(i, "push", f"s{i}", created=f"2026-11-12T{t}:00Z") for i, t in enumerate(times, 10)]
        jobs = {r["id"]: [job("a", HOSTED, 0, 1)] for r in listing}
        r = self.go(Fake(listing, jobs, limit=2), events=("push",))
        self.assertFalse(r["partial"])
        self.assertEqual(row(r, "push")["runs"], 6)

    def test_a_truncated_listing_that_cannot_be_split_is_partial(self):
        listing = [run(i, "push", f"s{i}") for i in range(10, 13)]  # three runs in the same second
        jobs = {r["id"]: [job("a", HOSTED, 0, 1)] for r in listing}
        r = self.go(Fake(listing, jobs, limit=2), events=("push",))
        self.assertTrue(r["partial"])
        self.assertTrue(any("truncated" in x for x in r["partial_reasons"]))

    def test_the_read_cap_makes_unread_runs_partial_and_cached_runs_are_kept(self):
        f = Fake(self.listing, self.jobs)
        r = self.go(f, cap=5)
        self.assertTrue(r["partial"])
        self.assertTrue(any("cap" in x for x in r["partial_reasons"]))
        self.assertLessEqual(r["reads"], 5)
        r2 = self.go(Fake(self.listing, self.jobs), cap=1000)
        self.assertFalse(r2["partial"])

    def test_an_open_day_is_not_cached(self):
        f = Fake(self.listing, self.jobs)
        self.go(f, now=dt.datetime(2026, 11, 12, 18, tzinfo=dt.timezone.utc))
        f2 = Fake(self.listing, self.jobs)
        self.go(f2, now=dt.datetime(2026, 11, 12, 18, tzinfo=dt.timezone.utc))
        self.assertTrue(any("/actions/runs?" in p for p in f2.paths))


if __name__ == "__main__":
    unittest.main()

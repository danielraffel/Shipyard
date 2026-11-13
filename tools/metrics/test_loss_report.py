import datetime as dt
import io
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import loss_report as lr  # noqa: E402

TICK = HERE.parent / "steward-tick" / "shipyard-steward-tick.sh"
UTC = dt.timezone.utc
REPO = "acme/repo"


def stamp(t):
    return t.strftime("%Y%m%dT%H%M%SZ")


def tick_json(rows):
    prs = []
    for number, head, token, actor, extra in rows:
        p = {"number": number, "head_sha": head, "blocked_on": token,
             "since": "2026-11-10T00:00:00Z",
             "next_action": {"actor": actor, "action": "x", "stop": actor == "owner"}}
        p.update(extra or {})
        prs.append(p)
    return json.dumps({"repos": [{"repo": REPO, "prs": prs, "errors": []}]})


class Fixture(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.now = dt.datetime(2026, 11, 13, 12, 0, tzinfo=UTC)

    def tearDown(self):
        self.tmp.cleanup()

    def live(self, t, rows, name=None):
        (self.dir / (name or f"acme_repo-{stamp(t)}.json")).write_text(tick_json(rows))

    def archive(self, day, members):
        (self.dir / "archive").mkdir(exist_ok=True)
        with tarfile.open(self.dir / "archive" / f"ticks-{day}.tar.gz", "w:gz") as tf:
            for name, body in members:
                data = body.encode()
                info = tarfile.TarInfo("./" + name)
                info.size = len(data)
                tf.addfile(info, io.BytesIO(data))

    def report(self, **kw):
        return lr.build_report(str(self.dir), dt.timedelta(days=4), self.now, **kw)


class EpisodeTests(Fixture):
    def test_episode_collapses_across_archive_and_live(self):
        t1 = dt.datetime(2026, 11, 9, 10, 0, tzinfo=UTC)
        t2 = t1 + dt.timedelta(minutes=30)
        t3 = dt.datetime(2026, 11, 13, 9, 0, tzinfo=UTC)
        rows = [(7, "aaa", "required_failed:review-verdict", "author", None)]
        self.archive("20261109", [(f"acme_repo-{stamp(t1)}.json", tick_json(rows)),
                                  (f"acme_repo-{stamp(t2)}.json", tick_json(rows))])
        self.live(t3, rows)
        r = lr.build_report(str(self.dir), dt.timedelta(days=5), self.now, gap_minutes=60 * 24 * 5)
        self.assertEqual(r["sources"]["archive_files"], 2)
        self.assertEqual(r["sources"]["live_files"], 1)
        self.assertEqual(r["totals"]["rows_observed"], 3)
        self.assertEqual(r["totals"]["episodes"], 1)

    def test_file_in_both_live_and_archive_is_read_once(self):
        t = dt.datetime(2026, 11, 11, 10, 0, tzinfo=UTC)
        rows = [(1, "a", "required_pending", "ci", None)]
        self.live(t, rows)
        self.archive("20261111", [(f"acme_repo-{stamp(t)}.json", tick_json(rows))])
        r = self.report()
        self.assertEqual(r["totals"]["rows_observed"], 1)

    def test_a_gap_ends_an_episode_instead_of_spanning_missing_days(self):
        rows = [(1, "a", "required_failed:macos", "author", None)]
        self.live(dt.datetime(2026, 11, 10, 1, 0, tzinfo=UTC), rows)
        self.live(dt.datetime(2026, 11, 12, 1, 0, tzinfo=UTC), rows)
        r = self.report()
        self.assertEqual(r["totals"]["episodes"], 2)
        self.assertEqual(r["agent_free_resolution"]["denominator_resolved_actionable"], 0)

    def test_errored_tick_is_a_gap_not_a_resolution(self):
        t = dt.datetime(2026, 11, 12, 1, 0, tzinfo=UTC)
        rows = [(1, "a", "required_failed:macos", "author", None)]
        self.live(t, rows)
        (self.dir / f"acme_repo-{stamp(t + dt.timedelta(minutes=6))}.json").write_text(
            json.dumps({"repos": [{"repo": REPO, "prs": [], "errors": ["boom"]}]}))
        self.live(t + dt.timedelta(minutes=12), rows)
        r = self.report()
        self.assertEqual(r["totals"]["episodes"], 1)
        self.assertEqual(r["agent_free_resolution"]["denominator_resolved_actionable"], 0)
        self.assertEqual(r["sources"]["repo_ticks_without_pr_list"], 1)
        self.assertEqual(r["coverage"]["window"]["status"], "partial")

    def test_malformed_tick_makes_coverage_partial_then_unavailable(self):
        rows = [(1, "a", "required_failed:macos", "author", None)]
        day = dt.datetime(2026, 11, 13, 1, 0, tzinfo=UTC)
        self.live(day, rows)
        self.live(day + dt.timedelta(hours=1), rows)
        clean = lr.build_report(str(self.dir), dt.timedelta(hours=12), self.now)
        self.assertEqual(clean["coverage"]["window"]["status"], "available")
        (self.dir / f"acme_repo-{stamp(day + dt.timedelta(minutes=90))}.json").write_text("{")
        r = lr.build_report(str(self.dir), dt.timedelta(hours=12), self.now)
        self.assertEqual(r["coverage"]["window"]["status"], "partial")
        for f in self.dir.glob("acme_repo-*.json"):
            if f.read_text() != "{":
                f.unlink()
        r = lr.build_report(str(self.dir), dt.timedelta(hours=12), self.now)
        self.assertEqual(r["coverage"]["window"]["status"], "unavailable")

    def test_required_pending_is_a_normal_wait_whatever_the_actor(self):
        self.live(dt.datetime(2026, 11, 12, 1, 0, tzinfo=UTC), [
            (1, "a", "required_pending", "author", None), (2, "b", "required_pending", "repair", None),
            (3, "c", "capacity_queued", "author", None)])
        self.assertEqual(self.report()["totals"]["by_class"], {"normal_wait": 3})

    def test_day_boundary_attributes_by_first_seen_utc_day(self):
        rows = [(3, "a", "required_failed:review-verdict", "author", None)]
        for t in (dt.datetime(2026, 11, 11, 23, 55, tzinfo=UTC), dt.datetime(2026, 11, 12, 0, 5, tzinfo=UTC)):
            self.live(t, rows)
        # the same PR later clears on the same head into a normal wait
        self.live(dt.datetime(2026, 11, 12, 0, 10, tzinfo=UTC), [(3, "a", "required_pending", "ci", None)])
        r = self.report()
        s = r["series_per_day"]["review_verdict_same_head_no_push"]
        self.assertEqual(s["2026-11-11"], 1)
        self.assertEqual(s["2026-11-12"], 0)

    def test_push_after_block_counts_as_fix_push_and_not_agent_free(self):
        t = dt.datetime(2026, 11, 12, 1, 0, tzinfo=UTC)
        self.live(t, [(5, "a", "required_failed:macos", "author", None)])
        self.live(t + dt.timedelta(minutes=5), [(5, "b", "required_pending", "ci", None)])
        r = self.report()
        self.assertEqual(r["series_per_day"]["required_failed_fix_pushes"]["2026-11-12"], 1)
        af = r["agent_free_resolution"]
        self.assertEqual((af["denominator_resolved_actionable"], af["same_head_no_comment"]), (1, 0))


class CoverageTests(Fixture):
    def test_unavailable_is_null_never_zero(self):
        rows = [(1, "a", "capacity_queued", "ci", None)]
        self.live(dt.datetime(2026, 11, 12, 1, 0, tzinfo=UTC), rows)
        r = self.report()
        self.assertEqual(r["coverage"]["github"]["status"], "unavailable")
        self.assertIsNone(r["totals"]["closed_unmerged_episodes"])
        self.assertIsNone(r["agent_free_resolution"]["repaired_dispute_comments_in_cache"])
        self.assertEqual(r["coverage"]["ci_compute"]["status"], "unavailable")
        # a day with no tick files is null, a day with ticks and no matches is 0
        s = r["series_per_day"]["capacity_queued_episodes"]
        self.assertIsNone(s["2026-11-10"])
        self.assertEqual(s["2026-11-12"], 1)
        self.assertEqual(r["coverage"]["window"]["status"], "partial")
        # capacity_queued is a normal wait, not actionable
        self.assertEqual(r["totals"]["by_class"], {"normal_wait": 1})

    def test_red_everywhere_is_unavailable_until_seen(self):
        self.live(dt.datetime(2026, 11, 12, 1, 0, tzinfo=UTC), [(1, "a", "required_pending", "ci", None)])
        r = self.report()
        self.assertIsNone(r["series_per_day"]["red_everywhere_episodes"])
        self.assertEqual(r["series_coverage"]["red_everywhere_episodes"]["status"], "unavailable")
        self.live(dt.datetime(2026, 11, 12, 2, 0, tzinfo=UTC),
                  [(2, "b", "required_failed:macos", "author", {"blocked_detail": "red_everywhere: macos"})])
        r = self.report()
        self.assertEqual(r["series_per_day"]["red_everywhere_episodes"]["2026-11-12"], 1)

    def test_github_cache_separates_abandoned_and_counts_comments(self):
        t = dt.datetime(2026, 11, 12, 1, 0, tzinfo=UTC)
        self.live(t, [(9, "a", "merge_state:DIRTY", "author", None), (10, "a", "required_failed:macos", "author", None)])
        self.live(t + dt.timedelta(minutes=5), [(10, "a", "required_pending", "ci", None)])
        cache = self.dir / "cache"
        cache.mkdir()
        # PR 9 vanished and closed unmerged
        (cache / "closed.json").write_text(json.dumps([{"number": 9, "state": "closed", "merged_at": None,
                                                         "base": {"repo": {"full_name": REPO}}}]))
        (cache / "c.json").write_text(json.dumps([{
            "id": 1, "issue_url": f"https://api.github.com/repos/{REPO}/issues/10",
            "created_at": "2026-11-12T01:02:00Z", "body": "repaired: reran the check"}]))
        r = self.report(github=str(cache))
        self.assertEqual(r["totals"]["by_class"].get("closed_unmerged"), 1)
        self.assertEqual(r["totals"]["closed_unmerged_episodes"], 1)
        af = r["agent_free_resolution"]
        self.assertEqual((af["denominator_resolved_actionable"], af["same_head_no_comment"]), (1, 0))
        self.assertEqual(r["coverage"]["github"]["status"], "partial")

    def test_owner_stops_per_100_prs(self):
        t = dt.datetime(2026, 11, 12, 1, 0, tzinfo=UTC)
        self.live(t, [(1, "a", "owner_decision", "owner", None)] + [(n, "a", "required_pending", "ci", None) for n in range(2, 5)])
        self.assertEqual(self.report()["owner_stops"]["per_100_prs"], 25.0)


class DailyTests(Fixture):
    def summary(self):
        p = self.dir / "summary.log"
        return p.read_text().splitlines() if p.exists() else []

    def test_runs_once_per_utc_day(self):
        self.live(dt.datetime(2026, 11, 13, 1, 0, tzinfo=UTC), [(1, "a", "required_failed:macos", "author", None)])
        self.assertEqual(lr.daily(str(self.dir), self.now), 0)
        self.assertEqual(lr.daily(str(self.dir), self.now + dt.timedelta(hours=3)), 0)
        lines = [l for l in self.summary() if " loss-report " in l]
        self.assertEqual(len(lines), 1, lines)
        self.assertIn("episodes=1", lines[0])
        self.assertTrue((self.dir / "loss-report-20261113.json").exists())
        self.assertTrue((self.dir / "loss-report-20261113.md").exists())
        self.assertEqual(json.loads((self.dir / "loss-report-20261113.json").read_text())["schema"], lr.SCHEMA)
        lr.daily(str(self.dir), self.now + dt.timedelta(days=1))
        self.assertEqual(len([l for l in self.summary() if " loss-report " in l]), 2)

    def test_failure_is_logged_once_and_returns_zero(self):
        (self.dir / "archive").mkdir()
        orig = lr.build_report
        lr.build_report = lambda *a, **k: (_ for _ in ()).throw(RuntimeError("boom"))
        try:
            self.assertEqual(lr.daily(str(self.dir), self.now), 0)
            self.assertEqual(lr.daily(str(self.dir), self.now), 0)
        finally:
            lr.build_report = orig
        errs = [l for l in self.summary() if "loss-report ERROR" in l]
        self.assertEqual(len(errs), 1)
        self.assertFalse((self.dir / ".loss-report.lock").exists())

    def test_corrupt_archive_and_missing_dir_do_not_raise(self):
        (self.dir / "archive").mkdir()
        (self.dir / "archive" / "ticks-20261112.tar.gz").write_text("not a tar")
        (self.dir / "acme_repo-20261112T010000Z.json").write_text("{")
        r = self.report()
        self.assertEqual(r["sources"]["bad_archives"], 1)
        self.assertEqual(r["sources"]["unreadable"], 1)
        self.assertEqual(r["coverage"]["window"]["status"], "unavailable")
        self.assertEqual(lr.daily(str(self.dir / "missing"), self.now), 0)


class TickIntegrationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.home = Path(self.tmp.name)
        local = self.home / ".local" / "bin"
        clones = self.home / ".local" / "share" / "shipyard-steward" / "repo"
        local.mkdir(parents=True)
        clones.mkdir(parents=True)
        (clones / ".git").mkdir()
        (self.home / ".local" / "share" / "shipyard-steward" / "fleet-repos.txt").write_text("acme/repo\n")
        (local / "gh").write_text("#!/bin/bash\nexit 0\n")
        (local / "ghapp").write_text("#!/bin/bash\nif [ \"$1\" = api ]; then echo 1000; fi\n")
        (local / "shipyard").write_text("#!/bin/bash\nif [ \"$1\" = --version ]; then echo 'shipyard 1.0'; exit 0; fi\n"
                                        "printf '%s\\n' '{\"repos\":[]}'\n")
        (local / "timeout").write_text("#!/bin/bash\nshift\nexec \"$@\"\n")
        for p in local.iterdir():
            p.chmod(0o755)
        self.log_dir = self.home / "Library" / "Logs" / "shipyard-steward-tick"
        self.log_dir.mkdir(parents=True)
        self.env = os.environ.copy()
        self.env.update(HOME=str(self.home), PATH=f"{local}:/usr/bin:/bin", SHIPYARD_STEWARD_INVARIANTS="0")

    def tearDown(self):
        self.tmp.cleanup()

    def tick(self, **env):
        e = self.env.copy()
        e.update(env)
        return subprocess.run(["bash", str(TICK)], env=e, text=True, capture_output=True)

    def wait_for(self, text, seconds=15):
        end = time.time() + seconds
        while time.time() < end:
            p = self.log_dir / "summary.log"
            if p.exists() and text in p.read_text():
                return True
            time.sleep(0.2)
        return False

    def test_tick_starts_the_report_detached_once_a_day(self):
        script = HERE / "loss_report.py"
        r = self.tick(SHIPYARD_LOSS_REPORT_SCRIPT=str(script))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue(self.wait_for(" loss-report "), "no loss-report line")
        self.tick(SHIPYARD_LOSS_REPORT_SCRIPT=str(script))
        time.sleep(1.5)
        text = (self.log_dir / "summary.log").read_text()
        self.assertEqual(text.count(" loss-report window="), 1, text)

    def test_a_failing_report_never_fails_the_tick(self):
        bad = self.home / "bad.py"
        bad.write_text("import sys\nsys.exit(7)\n")
        r = self.tick(SHIPYARD_LOSS_REPORT_SCRIPT=str(bad))
        self.assertEqual(r.returncode, 0, r.stderr)
        r = self.tick(SHIPYARD_LOSS_REPORT_SCRIPT=str(self.home / "missing.py"))
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_off_switch(self):
        r = self.tick(SHIPYARD_LOSS_REPORT="off", SHIPYARD_LOSS_REPORT_SCRIPT=str(HERE / "loss_report.py"))
        self.assertEqual(r.returncode, 0)
        time.sleep(1.5)
        self.assertNotIn("loss-report", (self.log_dir / "summary.log").read_text())


if __name__ == "__main__":
    unittest.main()

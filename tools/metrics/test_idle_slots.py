import argparse
import datetime as dt
import json
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import drain_watch as dw  # noqa: E402
import loss_report as lr  # noqa: E402

UTC = dt.timezone.utc
NOW = dt.datetime(2026, 11, 13, 12, 0, tzinfo=UTC)
REPO = "acme/repo"


def cap(free, verification="verified", host="h"):
    return {"verification": verification, "free_slots": free if verification == "verified" else None,
            "eligible_slots": free if verification == "verified" else None, "pool_state": "on", "host_id": host,
            "reason": None if verification == "verified" else "pool off", "schema": "tartci.fleet-capacity/v1"}


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        r = Path(self.tmp.name)
        self.log, self.stubs = r / "log", r / "stubs"
        self.log.mkdir()
        self.stubs.mkdir()
        for name, body in (("tartci", f'cat "{self.stubs}/local.json"'),
                           ("ssh", 'h=$5\n'
                                   f'[ -f "{self.stubs}/$h.json" ] || exit 255\ncat "{self.stubs}/$h.json"')):
            p = self.stubs / name
            p.write_text("#!/bin/bash\n" + body + "\n")
            p.chmod(0o755)
        self.args = argparse.Namespace(
            log_dir=str(self.log), fleet_dir=str(r / "fleet"), runner_dir=str(r / "runner"), state=str(r / "state.json"),
            summary=str(self.log / "summary.log"), ghapp="x", dry_run=False, from_start=False, idle_sample=True,
            probe_host=[], gate_target=["macos"], probe_repo=REPO, tartci=str(self.stubs / "tartci"),
            ssh=str(self.stubs / "ssh"), idle_cap_seconds=600)

    def tearDown(self):
        self.tmp.cleanup()

    def probe(self, host, value):
        (self.stubs / f"{host}.json").write_text(json.dumps(value))

    def tick(self, t, rows):
        prs = []
        for n, token, actor, extra in rows:
            p = {"number": n, "head_sha": "h", "blocked_on": token, "since": "2026-11-13T00:00:00Z",
                 "next_action": {"actor": actor, "action": "x", "stop": False}}
            p.update(extra or {})
            prs.append(p)
        (self.log / f"acme_repo-{t.strftime('%Y%m%dT%H%M%SZ')}.json").write_text(
            json.dumps({"repos": [{"repo": REPO, "prs": prs, "errors": []}]}))

    def samples(self):
        p = self.log / "idle-slots.jsonl"
        return [json.loads(l) for l in p.read_text().splitlines()] if p.exists() else []

    def sample_at(self, minutes):
        t = NOW + dt.timedelta(minutes=minutes)
        dw.run(self.args, t)
        return self.samples()[-1]


class WaitingStateTests(unittest.TestCase):
    def row(self, token, actor="author", loop=None, stop=False):
        return {"token": token, "actor": actor, "stop": stop, "loop_state": loop}

    def test_loop_state_wins_and_running_or_queued_do_not_wait(self):
        self.assertIsNone(dw.waiting_state(self.row("required_failed:macos", loop="running")))
        self.assertIsNone(dw.waiting_state(self.row("required_failed:macos", loop="queued")))
        for s in ("behind", "dirty", "red", "hold", "awaiting_review", "quarantined", "ejected"):
            self.assertEqual(dw.waiting_state(self.row("required_pending", "ci", loop=s)), s)

    def test_fallback_maps_blocked_on(self):
        w = dw.waiting_state
        self.assertEqual(w(self.row("merge_state:DIRTY")), "dirty")
        self.assertEqual(w(self.row("merge_state:BEHIND")), "behind")
        self.assertEqual(w(self.row("required_failed:macos,review-verdict")), "red")
        self.assertEqual(w(self.row("held:review-verdict")), "hold")
        self.assertEqual(w(self.row("ejected_same_head:failed_checks")), "ejected")
        self.assertEqual(w(self.row("no_verdict", "reviewer")), "awaiting_review")
        self.assertEqual(w(self.row("required_pending", "reviewer")), "awaiting_review")
        for token, actor in (("required_pending", "ci"), ("capacity_queued", "ci"), ("queued", "steward"),
                             ("draft", "author"), ("owner_decision", "owner"), ("ready", "steward")):
            self.assertIsNone(w(self.row(token, actor)), token)


class SamplerTests(Base):
    def test_minutes_are_idle_slots_times_interval_only_while_prs_wait(self):
        self.probe("local", cap(3))
        self.tick(NOW, [(1, "merge_state:DIRTY", "author", None), (2, "required_pending", "ci", None)])
        first = self.sample_at(0)
        self.assertFalse(first["counted"])                       # no previous sample: interval unknown
        self.tick(NOW + dt.timedelta(minutes=5), [(1, "merge_state:DIRTY", "author", None)])
        s = self.sample_at(5)
        self.assertEqual((s["idle_slots"], s["interval_s"], s["minutes"], s["counted"]), (3, 300, 15.0, True))
        self.assertEqual(s["waiting"], {"dirty": 1})
        # nobody waiting: idle slots are not counted
        self.tick(NOW + dt.timedelta(minutes=10), [(2, "required_pending", "ci", None), (3, "queued", "steward", None)])
        s = self.sample_at(10)
        self.assertEqual((s["idle_slots"], s["waiting_total"], s["minutes"], s["counted"]), (3, 0, 0.0, False))

    def test_hosts_are_summed_and_a_failed_probe_is_partial_not_zero(self):
        self.probe("local", cap(1))
        self.probe("m5s", cap(2, host="m5s"))
        self.args.probe_host = ["m5s", "m1"]                     # m1 has no stub: ssh exits 255
        self.tick(NOW, [(1, "required_failed:macos", "author", None)])
        self.sample_at(0)
        self.tick(NOW + dt.timedelta(minutes=5), [(1, "required_failed:macos", "author", None)])
        s = self.sample_at(5)
        self.assertEqual((s["idle_slots"], s["minutes"]), (3, 15.0))
        self.assertEqual([(p["host"], p["ok"]) for p in s["probes"]], [("local", True), ("m5s", True), ("m1", False)])

    def test_unverified_everywhere_is_unavailable_never_zero(self):
        self.probe("local", cap(0, verification="unknown"))
        self.tick(NOW, [(1, "required_failed:macos", "author", None)])
        self.sample_at(0)
        self.tick(NOW + dt.timedelta(minutes=5), [(1, "required_failed:macos", "author", None)])
        s = self.sample_at(5)
        self.assertIsNone(s["idle_slots"])
        self.assertFalse(s["counted"])
        rep = lr.idle_slots(str(self.log), NOW - dt.timedelta(hours=1), NOW + dt.timedelta(hours=1), ["2026-11-13"])
        self.assertIsNone(rep["2026-11-13"]["idle_slot_minutes"])
        self.assertEqual(rep["2026-11-13"]["coverage"]["status"], "unavailable")

    def test_interval_is_capped_after_a_gap(self):
        self.probe("local", cap(2))
        self.tick(NOW, [(1, "merge_state:DIRTY", "author", None)])
        self.sample_at(0)
        self.tick(NOW + dt.timedelta(minutes=60), [(1, "merge_state:DIRTY", "author", None)])
        s = self.sample_at(60)
        self.assertEqual((s["interval_s"], s["interval_capped"], s["minutes"]), (600, True, 20.0))

    def test_loop_state_from_the_tick_is_used_when_present(self):
        self.probe("local", cap(1))
        rows = [(1, "required_pending", "ci", {"loop": {"state": "red"}}), (2, "merge_state:DIRTY", "author", {"loop": {"state": "running"}})]
        self.tick(NOW, rows)
        self.sample_at(0)
        self.tick(NOW + dt.timedelta(minutes=5), rows)
        s = self.sample_at(5)
        self.assertEqual(s["waiting"], {"red": 1})
        self.assertTrue(s["loop_source"])

    def test_dry_run_writes_nothing_and_a_broken_probe_does_not_stop_the_watch(self):
        self.probe("local", cap(1))
        self.tick(NOW, [(1, "merge_state:DIRTY", "author", None)])
        self.args.dry_run = True
        _, actions, _ = dw.run(self.args, NOW)
        self.assertTrue(any(a.startswith("idle sample:") for a in actions))
        self.assertEqual(self.samples(), [])
        self.args.dry_run = False
        self.args.tartci = "/nonexistent/tartci"
        dw.run(self.args, NOW)
        self.assertFalse(self.samples()[-1]["probes"][0]["ok"])


class ReportTests(Base):
    def write(self, rows):
        with open(self.log / "idle-slots.jsonl", "w") as fh:
            for r in rows:
                fh.write(json.dumps(r) + "\n")

    def test_per_day_minutes_top_three_reasons_and_summary_line(self):
        def s(t, minutes, waiting, **kw):
            d = {"t": t, "interval_s": 300, "interval_capped": False, "idle_slots": 2, "waiting": waiting,
                 "waiting_total": sum(waiting.values()), "probes": [{"ok": True}], "minutes": minutes, "counted": minutes > 0}
            d.update(kw)
            return d
        self.write([
            s("2026-11-12T10:00:00+00:00", 10.0, {"dirty": 3, "red": 1}),
            s("2026-11-12T10:05:00+00:00", 10.0, {"hold": 1, "awaiting_review": 1, "ejected": 2, "dirty": 1}),
            s("2026-11-12T10:10:00+00:00", 0.0, {}),
        ])
        days = ["2026-11-11", "2026-11-12", "2026-11-13"]
        rep = lr.idle_slots(str(self.log), NOW - dt.timedelta(days=3), NOW, days)
        self.assertIsNone(rep["2026-11-11"])                      # no samples: null, not 0
        d = rep["2026-11-12"]
        self.assertEqual(d["idle_slot_minutes"], 20.0)
        self.assertEqual([(x["state"], x["minutes"]) for x in d["top_reasons"]], [("dirty", 9.5), ("ejected", 4.0), ("red", 2.5)])
        self.assertEqual(d["coverage"]["status"], "available")
        self.assertEqual(d["reasons_source"], "blocked_on mapping")
        line = lr.idle_summary_line({"idle_gate_slots": rep}, "20261113T000500Z", "2026-11-12")
        self.assertIn("idle-slots day=2026-11-12 idle_slot_minutes=20.0 top=dirty:47.5%,ejected:20.0%,red:12.5%", line)
        self.assertIn("coverage=available", line)
        self.assertIn("idle_slot_minutes=unavailable", lr.idle_summary_line({"idle_gate_slots": rep}, "x", "2026-11-11"))

    def test_daily_run_logs_the_idle_line_for_yesterday(self):
        self.write([{"t": "2026-11-12T10:00:00+00:00", "interval_s": 300, "interval_capped": False, "idle_slots": 1,
                     "waiting": {"red": 1}, "waiting_total": 1, "probes": [{"ok": True}], "minutes": 5.0, "counted": True}])
        lr.daily(str(self.log), NOW)
        text = (self.log / "summary.log").read_text()
        self.assertIn(" idle-slots day=2026-11-12 idle_slot_minutes=5.0 top=red:100.0%", text)
        rep = json.loads((self.log / "loss-report-20261113.json").read_text())
        self.assertEqual(rep["idle_gate_slots"]["2026-11-12"]["idle_slot_minutes"], 5.0)

    def test_failed_probes_make_the_day_partial(self):
        self.write([{"t": "2026-11-12T10:00:00+00:00", "interval_s": 300, "interval_capped": False, "idle_slots": 1,
                     "waiting": {"red": 1}, "waiting_total": 1, "probes": [{"ok": True}, {"ok": False}], "minutes": 5.0, "counted": True}])
        rep = lr.idle_slots(str(self.log), NOW - dt.timedelta(days=2), NOW, ["2026-11-12"])
        self.assertEqual(rep["2026-11-12"]["coverage"]["status"], "partial")


if __name__ == "__main__":
    unittest.main()

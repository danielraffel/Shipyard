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
            ssh=str(self.stubs / "ssh"))

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
    def row(self, token, actor="author", loop=None, action=None, detail=None):
        return {"token": token, "actor": actor, "stop": False, "loop_state": loop, "decision_action": action, "detail": detail}

    def test_matches_the_coordinators_definition(self):
        # (blocked_on, decision.action, expected) cases from idle_gate.py at pulp-planning 1200a0775;
        # the same function was also checked row for row against 8,853 real tick rows.
        cases = [
            ("queued", None, None), ("admitted", None, None), ("draft", None, None), ("required_pending", None, None),
            ("excluded_label:superseded", None, None), ("owner_decision", "excluded_label", None),
            ("required_failed:macos", "waiting_required", None),
            ("capacity_queued", "waiting_required", "awaiting_slot"),
            ("merge_state:DIRTY", None, "dirty"), ("merge_state:BEHIND", None, "behind"), ("merge_state:UNSTABLE", None, "behind"),
            ("held:review-verdict", None, "held"), ("ejected_same_head:failed_checks", None, "ejected"),
            ("quarantine_x", None, "quarantined"), ("required_failed:review-verdict", None, "awaiting_review"),
            ("no_verdict", None, "awaiting_review"), ("carries_unapproved_commits:#1", None, "awaiting_review"),
            ("required_never_ran:review-verdict", None, "never_ran"), ("required_failed:macos,review-verdict", None, "red"),
            ("unmanaged", None, "unmanaged"), ("direct_merge_refused", None, "direct_merge_refused"), ("ready", None, "ready"),
        ]
        for token, action, want in cases:
            self.assertEqual(dw.waiting_state(self.row(token, action=action)), want, token)

    def test_loop_state_wins_and_running_or_queued_do_not_wait(self):
        self.assertIsNone(dw.waiting_state(self.row("required_failed:macos", loop="running")))
        self.assertIsNone(dw.waiting_state(self.row("required_failed:macos", loop="queued")))
        self.assertEqual(dw.waiting_state(self.row("required_pending", "ci", loop="hold")), "held")
        for s in ("behind", "dirty", "red", "awaiting_review", "quarantined", "ejected"):
            self.assertEqual(dw.waiting_state(self.row("required_pending", "ci", loop=s)), s)


class SamplerTests(Base):
    def test_sample_records_idle_slots_and_per_repo_waiting_states(self):
        self.probe("local", cap(3))
        self.tick(NOW, [(1, "merge_state:DIRTY", "author", None), (2, "required_pending", "ci", None),
                        (3, "capacity_queued", "ci", None)])
        s = self.sample_at(0)
        self.assertEqual(s["idle_slots"], 3)
        self.assertEqual(s["waiting"], {REPO: {"dirty": 1, "awaiting_slot": 1}})

    def test_hosts_are_summed_and_a_failed_probe_is_kept_not_zeroed(self):
        self.probe("local", cap(1))
        self.probe("m5s", cap(2, host="m5s"))
        self.args.probe_host = ["m5s", "m1"]                     # m1 has no stub: ssh exits 255
        self.tick(NOW, [(1, "required_failed:macos", "author", None)])
        s = self.sample_at(0)
        self.assertEqual(s["idle_slots"], 3)
        self.assertEqual([(p["host"], p["ok"]) for p in s["probes"]], [("local", True), ("m5s", True), ("m1", False)])

    def test_unverified_everywhere_is_null_never_zero(self):
        self.probe("local", cap(0, verification="unknown"))
        self.tick(NOW, [(1, "required_failed:macos", "author", None)])
        self.assertIsNone(self.sample_at(0)["idle_slots"])

    def test_loop_state_from_the_tick_is_used_when_present(self):
        self.probe("local", cap(1))
        self.tick(NOW, [(1, "required_pending", "ci", {"loop": {"state": "red"}}),
                        (2, "merge_state:DIRTY", "author", {"loop": {"state": "running"}})])
        s = self.sample_at(0)
        self.assertEqual(s["waiting"], {REPO: {"red": 1}})
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


def sample(t, idle, waiting, probes=None, **kw):
    d = {"t": t, "idle_slots": idle, "waiting": waiting, "probes": probes if probes is not None else [{"ok": idle is not None}],
         "stale_repos": [], "loop_source": False}
    d.update(kw)
    return d


class ReportTests(Base):
    def write(self, rows):
        with open(self.log / "idle-slots.jsonl", "w") as fh:
            for r in rows:
                fh.write(json.dumps(r) + "\n")

    def rep(self, days=("2026-11-11", "2026-11-12", "2026-11-13")):
        return lr.idle_slots(str(self.log), NOW - dt.timedelta(days=3), NOW, list(days))

    def test_minutes_weight_is_time_to_the_next_sample_and_only_while_the_repo_waits(self):
        a, b = "acme/a", "acme/b"
        self.write([
            sample("2026-11-12T10:00:00+00:00", 2, {a: {"held": 3, "red": 1}, b: {}}),        # 5 min, a waits
            sample("2026-11-12T10:05:00+00:00", 0, {a: {"red": 4}}),                          # no idle slots
            sample("2026-11-12T10:10:00+00:00", 2, {a: {}, b: {"dirty": 2}}),                 # 20 min, b waits
            sample("2026-11-12T10:30:00+00:00", 1, {a: {"red": 1}}),                          # last: weighs nothing
        ])
        v = self.rep()["2026-11-12"]
        self.assertEqual(v["repos"][a]["idle_slot_minutes"], 10.0)
        self.assertEqual(v["repos"][a]["top_states_pr_minutes"], [{"state": "held", "pr_minutes": 15.0}, {"state": "red", "pr_minutes": 5.0}])
        self.assertEqual(v["repos"][b]["idle_slot_minutes"], 40.0)
        self.assertEqual(v["repos"][b]["top_states_pr_minutes"], [{"state": "dirty", "pr_minutes": 40.0}])
        self.assertEqual(v["all_repos"]["idle_slot_minutes"], 50.0)      # a sample counts once when any repo waits
        self.assertEqual(v["coverage"]["status"], "available")
        self.assertEqual(v["waiting_source"], "blocked_on")

    def test_the_tick_weight_is_capped_at_thirty_minutes(self):
        self.write([sample("2026-11-12T10:00:00+00:00", 1, {"a": {"red": 1}}), sample("2026-11-12T12:00:00+00:00", 1, {"a": {}})])
        v = self.rep()["2026-11-12"]
        self.assertEqual(v["repos"]["a"]["idle_slot_minutes"], 30.0)
        self.assertEqual(v["coverage"]["status"], "partial")

    def test_top_three_states_and_null_and_unavailable_days(self):
        w = {"a": {"held": 5, "red": 4, "dirty": 3, "unmanaged": 1}}
        self.write([sample("2026-11-12T10:00:00+00:00", 1, w), sample("2026-11-12T10:05:00+00:00", 1, {}),
                    sample("2026-11-13T10:00:00+00:00", None, w)])
        r = self.rep()
        self.assertIsNone(r["2026-11-11"])                                              # no samples: null, not 0
        self.assertEqual([x["state"] for x in r["2026-11-12"]["repos"]["a"]["top_states_pr_minutes"]], ["held", "red", "dirty"])
        self.assertEqual(r["2026-11-13"]["coverage"]["status"], "unavailable")
        self.assertIsNone(r["2026-11-13"]["all_repos"]["idle_slot_minutes"])

    def test_failed_probe_makes_the_day_partial(self):
        self.write([sample("2026-11-12T10:00:00+00:00", 1, {"a": {"red": 1}}, probes=[{"ok": True}, {"ok": False}]),
                    sample("2026-11-12T10:05:00+00:00", 1, {})])
        self.assertEqual(self.rep()["2026-11-12"]["coverage"]["status"], "partial")

    def test_summary_lines_and_the_daily_run(self):
        self.write([sample("2026-11-12T10:00:00+00:00", 2, {"acme/a": {"held": 2382, "red": 2024, "unmanaged": 606, "x": 1}}),
                    sample("2026-11-12T10:05:00+00:00", 2, {})])
        lr.daily(str(self.log), NOW)
        text = (self.log / "summary.log").read_text()
        self.assertIn("idle-slots day=2026-11-12 repo=all idle_slot_minutes=10.0 top=held:11910,red:10120,unmanaged:3030", text)
        self.assertIn("idle-slots day=2026-11-12 repo=acme/a idle_slot_minutes=10.0 ", text)
        rep = json.loads((self.log / "loss-report-20261113.json").read_text())
        self.assertEqual(rep["idle_gate_slots"]["2026-11-12"]["repos"]["acme/a"]["idle_slot_minutes"], 10.0)
        self.assertIn("repo=all idle_slot_minutes=unavailable", lr.idle_summary_lines({"idle_gate_slots": {"d": None}}, "x", "d")[0])


if __name__ == "__main__":
    unittest.main()

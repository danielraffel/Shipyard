import argparse
import datetime as dt
import json
import os
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import drain_watch as dw  # noqa: E402

UTC = dt.timezone.utc
REPO = "acme/repo"
NOW = dt.datetime(2026, 11, 13, 12, 0, tzinfo=UTC)

STUB = r'''#!/usr/bin/env python3
import json, sys
store = sys.argv[0] + ".store"
try:
    db = json.load(open(store))
except Exception:
    db = {"issues": [], "calls": []}
a = sys.argv[2:]
db["calls"].append(a)
path = a[0]
method = a[a.index("-X") + 1] if "-X" in a else ("POST" if "-f" in a else "GET")
fields = {}
for i, x in enumerate(a):
    if x == "-f":
        k, _, v = a[i + 1].partition("=")
        fields.setdefault(k, v)
out = {}
if method == "GET":
    out = db["issues"]
elif path.endswith("/comments"):
    out = {}
elif method == "POST":
    n = len(db["issues"]) + 100
    db["issues"].append({"number": n, "state": "open", "body": fields["body"], "title": fields["title"], "labels": fields.get("labels[]")})
    out = {"number": n}
else:
    n = int(path.rsplit("/", 1)[1])
    for it in db["issues"]:
        if it["number"] == n:
            if "body" in fields: it["body"] = fields["body"]
            if "state" in fields: it["state"] = fields["state"]
    out = {"number": n}
json.dump(db, open(store, "w"))
print(json.dumps(out))
'''


def stamp(t):
    return t.strftime("%Y%m%dT%H%M%SZ")


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        r = Path(self.tmp.name)
        self.log, self.fleet, self.runner = r / "log", r / "fleet", r / "runner"
        for d in (self.log, self.fleet, self.runner / "repair-claims"):
            d.mkdir(parents=True)
        self.ghapp = r / "ghapp"
        self.ghapp.write_text(STUB)
        self.ghapp.chmod(0o755)
        self.args = argparse.Namespace(log_dir=str(self.log), fleet_dir=str(self.fleet), runner_dir=str(self.runner),
                                       state=str(r / "state.json"), summary=str(self.log / "summary.log"),
                                       ghapp=str(self.ghapp), dry_run=False, from_start=False)

    def tearDown(self):
        self.tmp.cleanup()

    def tick(self, t, rows, repo=REPO):
        prs = []
        for n, head, token, actor, since in rows:
            prs.append({"number": n, "head_sha": head, "blocked_on": token, "since": since.isoformat().replace("+00:00", "Z"),
                        "next_action": {"actor": actor, "action": "x", "stop": actor == "owner"}})
        (self.log / f"{repo.replace('/', '_')}-{stamp(t)}.json").write_text(json.dumps({"repos": [{"repo": repo, "prs": prs, "errors": []}]}))

    def history(self, rows_by_offset, minutes=150, step=15):
        """ticks every `step` minutes back to `minutes` ago; rows_by_offset(minutes_ago) -> rows"""
        for m in range(minutes, -1, -step):
            self.tick(NOW - dt.timedelta(minutes=m), rows_by_offset(m))

    def run_once(self, now=NOW):
        return dw.run(self.args, now)

    def summary(self):
        p = self.log / "summary.log"
        return p.read_text().splitlines() if p.exists() else []

    def state(self):
        with open(self.args.state) as fh:
            return json.load(fh)

    def store(self):
        p = Path(str(self.ghapp) + ".store")
        return json.loads(p.read_text()) if p.exists() else {"issues": [], "calls": []}


class DetectionTests(Base):
    def test_drain_stall_detected_only_when_nothing_merged(self):
        old = NOW - dt.timedelta(hours=3)
        self.history(lambda m: [(1, "a", "required_failed:macos", "author", old)])
        _, _, f = self.run_once()
        self.assertEqual([x["kind"] for x in f], ["drain_stall"])

    def test_a_merge_in_the_window_is_not_a_stall(self):
        old = NOW - dt.timedelta(hours=3)
        # PR 2 is queued, then leaves the list an hour ago: a merge
        self.history(lambda m: [(1, "a", "required_failed:macos", "author", old)] + ([(2, "b", "queued", "steward", old)] if m > 60 else []))
        _, _, f = self.run_once()
        self.assertEqual(f, [])

    def _history_with_error(self, error):
        old = NOW - dt.timedelta(hours=3)
        for m in range(150, -1, -15):
            t = NOW - dt.timedelta(minutes=m)
            pr = {"number": 1, "head_sha": "a", "blocked_on": "required_failed:macos",
                  "since": old.isoformat().replace("+00:00", "Z"),
                  "next_action": {"actor": "author", "action": "x", "stop": False}}
            if error and m <= 30:
                pr["error"] = error
            (self.log / f"acme_repo-{stamp(t)}.json").write_text(
                json.dumps({"repos": [{"repo": REPO, "prs": [pr], "errors": []}]}))

    def test_a_read_deferred_pr_is_unobserved_not_a_stall_input(self):
        # Control: the same history with no error is a stall.
        self._history_with_error(None)
        _, _, f = self.run_once()
        self.assertEqual([x["kind"] for x in f], ["drain_stall"])

    def test_a_pr_unread_in_the_latest_tick_is_not_counted_as_waiting(self):
        self._history_with_error("queue-arm-guard: cannot read live PR state: gh: HTTP 503")
        _, _, f = self.run_once()
        self.assertEqual(f, [])

    def test_a_policy_error_is_still_observed(self):
        self._history_with_error("needs an exact-head approval")
        _, _, f = self.run_once()
        self.assertEqual([x["kind"] for x in f], ["drain_stall"])

    def test_read_deferred_classifier(self):
        self.assertTrue(dw.read_deferred({"error": "gh: HTTP 503"}))
        self.assertTrue(dw.read_deferred({"error": "x", "error_kind": "read"}))
        self.assertFalse(dw.read_deferred({"error": "HTTP 503", "error_kind": "policy"}))
        self.assertFalse(dw.read_deferred({"error": "needs an exact-head approval"}))
        self.assertFalse(dw.read_deferred({"number": 1}))

    def test_normal_waits_and_short_history_do_not_stall(self):
        old = NOW - dt.timedelta(hours=3)
        self.history(lambda m: [(1, "a", "required_pending", "author", old)])
        self.assertEqual(self.run_once()[2], [])
        for f in self.log.glob("*.json"):
            f.unlink()
        self.history(lambda m: [(1, "a", "required_failed:macos", "author", old)], minutes=30)
        self.assertEqual(self.run_once()[2], [])

    def test_stale_hold(self):
        self.tick(NOW, [(7, "h", "held:review-verdict", "author", NOW - dt.timedelta(minutes=50)),
                        (8, "h", "held:review-verdict", "author", NOW - dt.timedelta(minutes=10))])
        _, _, f = self.run_once()
        self.assertEqual([(x["kind"], x["detail"].split(":")[1].strip()) for x in f], [("stale_hold", "#7")])

    def test_stuck_claim_needs_age_and_unmoved_head(self):
        self.tick(NOW, [(5, "H1", "required_failed:macos", "repair", NOW - dt.timedelta(hours=1)),
                        (6, "H3", "required_failed:macos", "repair", NOW - dt.timedelta(hours=1))])
        (self.runner / "repair-attempts.log").write_text(
            "1 acme/repo#5 H1 code\n2 acme/repo#6 H2 code\n")
        for pr in (5, 6):
            d = self.runner / "repair-claims" / f"acme_repo_{pr}"
            d.mkdir()
            (d / "pid").write_text("1\n")
            old = (NOW - dt.timedelta(hours=1)).timestamp()
            os.utime(d / "pid", (old, old))
        # claim for PR 7 is fresh
        d = self.runner / "repair-claims" / "acme_repo_7"
        d.mkdir()
        (d / "pid").write_text("1\n")
        fresh = (NOW - dt.timedelta(minutes=5)).timestamp()
        os.utime(d / "pid", (fresh, fresh))
        _, _, f = self.run_once()
        self.assertEqual([x["key"] for x in f if x["kind"] == "stuck_claim"], ["stuck_claim:acme/repo#5"])  # 6 pushed, 7 fresh

    def test_pass_faults_per_signature_and_lane(self):
        (self.fleet / "repair.log").write_text(
            "ok\nPermissionError: [Errno 1] Operation not permitted: '/x/.cache/ghread/a'\nfine\n")
        (self.fleet / "reviewer.launchd.log").write_text("fleet-pass.sh: line 3: rg: command not found\nUnable to read current working directory\n")
        (self.fleet / "reviewer.log").write_text("Traceback (most recent call last):\n")
        self.args.from_start = True
        _, _, f = self.run_once()
        got = {(x["lane"], x["detail"]) for x in f}
        self.assertIn(("repair", "PermissionError x1"), got)
        self.assertIn(("repair", "Operation not permitted x1"), got)
        self.assertIn(("reviewer", "command not found x1"), got)
        self.assertIn(("reviewer", "Unable to read current working directory x1"), got)
        self.assertIn(("reviewer", "Traceback x1"), got)


class TranscriptAndTimeTests(Base):
    def test_transcripts_are_not_scanned_but_runner_lines_are(self):
        (self.fleet / "repair.last-output.txt").write_text(
            "the agent wrote: Traceback (most recent call last)\nand a getcwd error, quoting PermissionError\n")
        (self.fleet / "repair.last-output.txt.codex-failed").write_text("Traceback\n")
        (self.fleet / "repair.launchd.log").write_text("fleet-pass.sh: line 9: rg: command not found\n")
        self.args.from_start = True
        _, _, f = self.run_once()
        self.assertEqual([(x["lane"], x["detail"]) for x in f], [("repair", "command not found x1")])

    def test_timestamps_attribute_a_line_to_its_own_or_the_preceding_time(self):
        log = self.fleet / "repair.log"
        log.write_text("2026-11-13T10:00:00Z role=repair ok\n")
        self.run_once()                                    # seed; last_run = NOW
        old = "2026-11-10T09:00:00Z"
        new = "2026-11-13T11:55:00Z"
        with open(log, "a") as fh:
            fh.write(f"{old} role=repair quoted old PermissionError\n"
                     "  continuation line with Traceback\n"           # belongs to the old line
                     f"{new} role=repair new getcwd failure\n"
                     "  second line: Operation not permitted\n"       # belongs to the new line
                     "role=repair untimestamped but after a new line: command not found\n")
        _, _, f = self.run_once()
        got = sorted(x["detail"] for x in f)
        self.assertEqual(got, ["Operation not permitted x1", "command not found x1", "getcwd x1"])

    def test_preceding_timestamp_carries_across_runs(self):
        log = self.fleet / "repair.log"
        log.write_text("2026-11-01T00:00:00Z old entry\n")
        self.run_once()
        with open(log, "a") as fh:
            fh.write("  stack continues: Traceback\n")        # still the old entry's time
        self.assertEqual(self.run_once(NOW + dt.timedelta(minutes=5))[2], [])


class StateTests(Base):
    def stalled(self):
        old = NOW - dt.timedelta(hours=3)
        self.history(lambda m: [(1, "a", "required_failed:macos", "author", old)])

    def test_announced_once_then_counted(self):
        self.stalled()
        lines1, _, _ = self.run_once()
        lines2, _, _ = self.run_once()
        self.assertEqual(len(lines1), 1)
        self.assertIn("kind=drain_stall repo=acme/repo lane=- ", lines1[0])
        self.assertIn("count=1", lines1[0])
        self.assertEqual(lines2, [])
        state = self.state()
        self.assertEqual(state["findings"]["drain_stall:acme/repo"]["runs"], 2)
        self.assertEqual(len([l for l in self.summary() if "drain-watch" in l]), 1)

    def test_issue_on_second_run_only_and_deduped(self):
        self.stalled()
        self.run_once()
        self.assertEqual(self.store()["issues"], [])
        self.run_once()
        issues = self.store()["issues"]
        self.assertEqual(len(issues), 1)
        self.assertIn("<!-- drain-watch:drain_stall:acme/repo -->", issues[0]["body"])
        self.assertEqual(issues[0]["labels"], "drain-watch")
        # lose the state: the marker finds the existing issue instead of opening a duplicate
        os.remove(self.args.state)
        self.run_once()
        self.run_once()
        self.assertEqual(len(self.store()["issues"]), 1)

    def test_closes_after_three_clear_runs(self):
        self.stalled()
        self.run_once()
        self.run_once()
        for f in self.log.glob("*.json"):
            f.unlink()
        self.tick(NOW, [])
        self.run_once()
        self.run_once()
        self.assertEqual(self.store()["issues"][0]["state"], "open")
        self.run_once()
        self.assertEqual(self.store()["issues"][0]["state"], "closed")
        self.assertEqual(self.state()["findings"], {})

    def test_dry_run_changes_nothing(self):
        self.stalled()
        self.run_once()
        self.args.dry_run = True
        lines, actions, _ = self.run_once()
        self.assertEqual(actions, ["would file/update issue for drain_stall:acme/repo"])
        self.assertEqual(self.store()["issues"], [])
        self.assertEqual(self.state()["findings"]["drain_stall:acme/repo"]["runs"], 1)

    def test_offsets_never_recount_old_lines(self):
        log = self.fleet / "repair.log"
        log.write_text("old PermissionError line\n")
        _, _, f = self.run_once()            # first ever run seeds at the end
        self.assertEqual(f, [])
        _, _, f = self.run_once()
        self.assertEqual(f, [])
        with open(log, "a") as fh:
            fh.write("new PermissionError line\npartial no newline PermissionError")
        _, _, f = self.run_once()
        self.assertEqual([x["detail"] for x in f], ["PermissionError x1"])  # partial line waits for its newline
        _, _, f = self.run_once()
        self.assertEqual(f, [])
        with open(log, "a") as fh:
            fh.write("\n")
        _, _, f = self.run_once()
        self.assertEqual([x["detail"] for x in f], ["PermissionError x1"])
        log.write_text("rewritten smaller\nPermissionError again\n")   # truncation resets
        _, _, f = self.run_once()
        self.assertEqual([x["detail"] for x in f], ["PermissionError x1"])

    def test_failed_issue_call_is_retried_with_original_evidence(self):
        log = self.fleet / "repair.log"
        log.write_text("start\n")
        self.run_once()                                   # seeds offsets
        good = self.args.ghapp
        with open(log, "a") as fh:
            fh.write("PermissionError: first sighting evidence\n")
        self.run_once()                                   # run 1 of the finding
        with open(log, "a") as fh:
            fh.write("PermissionError: second sighting\n")
        self.args.ghapp = "/nonexistent/ghapp"
        self.run_once()                                   # run 2: issue call fails
        self.assertTrue(any("ERROR issue" in l for l in self.summary()))
        self.assertIn("pending", self.state()["findings"]["pass_fault:repair:PermissionError"])
        self.args.ghapp = good
        _, _, f = self.run_once()                         # run 3: no new log lines, offsets are past them
        self.assertEqual(f, [])
        issues = self.store()["issues"]
        self.assertEqual(len(issues), 1)
        self.assertIn("first sighting evidence", issues[0]["body"])
        self.assertNotIn("pending", self.state()["findings"]["pass_fault:repair:PermissionError"])
        self.run_once()
        self.assertEqual(len(self.store()["issues"]), 1)

    def test_issue_failure_is_logged_not_raised(self):
        self.stalled()
        self.args.ghapp = "/nonexistent/ghapp"
        self.run_once()
        self.run_once()
        self.assertTrue(any("ERROR issue" in l for l in self.summary()))


class TickSafetyTests(Base):
    def cli(self, *extra, env=None):
        e = os.environ.copy()
        e.update(env or {})
        return subprocess.run([sys.executable, "-I", str(HERE / "drain_watch.py"), "--tick", "--log-dir", str(self.log),
                               "--fleet-dir", str(self.fleet), "--runner-dir", str(self.runner), *extra],
                              capture_output=True, text=True, env=e)

    def test_failing_run_never_fails_and_is_logged(self):
        (self.log / "drain-watch-state.json").mkdir()  # state path is a directory: save fails
        old = NOW - dt.timedelta(hours=3)
        self.history(lambda m: [(1, "a", "required_failed:macos", "author", old)])
        r = self.cli()
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertFalse((self.log / ".drain-watch.lock").exists())

    def test_missing_directories_and_busy_lock(self):
        r = subprocess.run([sys.executable, "-I", str(HERE / "drain_watch.py"), "--tick", "--log-dir", str(self.log / "none"),
                            "--fleet-dir", "/nonexistent", "--runner-dir", "/nonexistent"], capture_output=True, text=True)
        self.assertEqual(r.returncode, 0)
        (self.log / ".drain-watch.lock").mkdir()
        self.assertEqual(self.cli().returncode, 0)


class TickIntegrationTests(unittest.TestCase):
    TICK = HERE.parent / "steward-tick" / "shipyard-steward-tick.sh"

    def test_tick_starts_watch_detached_and_survives_its_failure(self):
        tmp = tempfile.TemporaryDirectory()
        home = Path(tmp.name)
        local = home / ".local" / "bin"
        clones = home / ".local" / "share" / "shipyard-steward" / "repo"
        local.mkdir(parents=True)
        clones.mkdir(parents=True)
        (clones / ".git").mkdir()
        (home / ".local" / "share" / "shipyard-steward" / "fleet-repos.txt").write_text("acme/repo\n")
        for name, body in (("gh", "exit 0"), ("ghapp", "if [ \"$1\" = api ]; then echo 1000; fi"),
                           ("shipyard", "if [ \"$1\" = --version ]; then echo 'shipyard 1.0'; exit 0; fi\necho '{\"repos\":[]}'"),
                           ("timeout", "shift\nexec \"$@\"")):
            (local / name).write_text("#!/bin/bash\n" + body + "\n")
            (local / name).chmod(0o755)
        log = home / "Library" / "Logs" / "shipyard-steward-tick"
        log.mkdir(parents=True)
        marker = home / "ran"
        fake = home / "fake_watch.py"
        fake.write_text(f"import sys\nopen({str(marker)!r},'w').write(' '.join(sys.argv[1:]))\nsys.exit(9)\n")
        env = os.environ.copy()
        env.update(HOME=str(home), PATH=f"{local}:/usr/bin:/bin", SHIPYARD_STEWARD_INVARIANTS="0",
                   SHIPYARD_LOSS_REPORT="off", SHIPYARD_DRAIN_WATCH_SCRIPT=str(fake))
        r = subprocess.run(["bash", str(self.TICK)], env=env, text=True, capture_output=True)
        self.assertEqual(r.returncode, 0, r.stderr)
        end = time.time() + 15
        while time.time() < end and not marker.exists():
            time.sleep(0.2)
        self.assertTrue(marker.exists())
        self.assertIn("--tick", marker.read_text())
        env["SHIPYARD_DRAIN_WATCH"] = "off"
        marker.unlink()
        self.assertEqual(subprocess.run(["bash", str(self.TICK)], env=env, capture_output=True).returncode, 0)
        time.sleep(1)
        self.assertFalse(marker.exists())
        tmp.cleanup()


if __name__ == "__main__":
    unittest.main()

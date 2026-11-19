import json
import os
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("shipyard-steward-tick.sh")


class StewardTickArchiveTests(unittest.TestCase):
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
        (local / "shipyard").write_text(
            "#!/bin/bash\n"
            "if [ \"$1\" = --version ]; then echo 'shipyard 1.0'; exit 0; fi\n"
            "printf '%s\\n' '{\"repos\":[]}'\n"
        )
        (local / "timeout").write_text("#!/bin/bash\nshift\nexec \"$@\"\n")
        for path in local.iterdir():
            path.chmod(0o755)
        self.env = os.environ.copy()
        self.env.update(
            HOME=str(self.home),
            PATH=f"{local}:/usr/bin:/bin",
            SHIPYARD_STEWARD_INVARIANTS="0",
        )
        self.log_dir = self.home / "Library" / "Logs" / "shipyard-steward-tick"
        self.log_dir.mkdir(parents=True)

    def tearDown(self):
        self.tmp.cleanup()

    def run_tick(self, **env):
        current = self.env.copy()
        current.update(env)
        return subprocess.run(["bash", str(SCRIPT)], env=current, text=True, capture_output=True)

    def test_fleet_report_publish_is_disabled_without_issue_variable(self):
        result = self.run_tick()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("fleet-report", (self.log_dir / "summary.log").read_text())

    def add_old_tick(self, name="old.json", days=5):
        path = self.log_dir / name
        path.write_text('{"blocked_on":"test"}\n')
        old = time.time() - days * 86400
        os.utime(path, (old, old))
        return path

    def test_archives_old_files_and_keeps_today(self):
        result = self.run_tick()
        self.assertEqual(result.returncode, 0, result.stderr)
        today = next(self.log_dir.glob("*.json"))
        old = self.add_old_tick()
        result = self.run_tick()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(today.exists())
        self.assertFalse(old.exists())
        archives = list((self.log_dir / "archive").glob("ticks-*.tar.gz"))
        self.assertEqual(len(archives), 1)
        with tarfile.open(archives[0], "r:gz") as archive:
            self.assertIn("./old.json", archive.getnames())

    def test_release_canary_runs_detached_and_logs_only_real_results(self):
        shipyard = self.home / ".local" / "bin" / "shipyard"
        shipyard.write_text(
            "#!/bin/bash\n"
            "if [ \"$1\" = --version ]; then echo 'shipyard 1.0'; exit 0; fi\n"
            "if [ \"$1\" = release-canary ]; then\n"
            "  [ \"$3\" = --help ] && exit 0\n"
            "  echo \"$*\" > \"$HOME/canary-args\"\n"
            "  echo 'release-canary status=fail tag=v1.0.1 issue=opened #5 exit 1'; exit 1\n"
            "fi\n"
            "printf '%s\\n' '{\"repos\":[]}'\n"
        )
        result = self.run_tick()
        self.assertEqual(result.returncode, 0, result.stderr)
        summary = self.log_dir / "summary.log"
        for _ in range(100):
            if "release-canary status=fail" in summary.read_text():
                break
            time.sleep(0.1)
        self.assertIn("acme/repo release-canary status=fail tag=v1.0.1", summary.read_text())
        self.assertIn("--repo acme/repo --daily --apply", (self.home / "canary-args").read_text())

    def test_loss_report_gets_the_five_repos_a_sized_cap_and_keeps_its_stderr(self):
        fake = self.home / "fake_loss_report.py"
        fake.write_text(
            "import os, sys\n"
            "open(os.path.join(os.environ['HOME'], 'lr-args'), 'w').write(' '.join(sys.argv[1:]))\n"
            "sys.stderr.write('boom from loss report\\n')\n"
        )
        result = self.run_tick(SHIPYARD_LOSS_REPORT_SCRIPT=str(fake))
        self.assertEqual(result.returncode, 0, result.stderr)
        err = self.log_dir / "loss-report.err"
        for _ in range(100):
            if err.exists() and "boom" in err.read_text():
                break
            time.sleep(0.1)
        self.assertIn("boom from loss report", err.read_text())
        args = (self.home / "lr-args").read_text()
        self.assertIn("--read-cap 1500", args)
        self.assertIn("--repos Generous-Corp/pulp,Generous-Corp/vellum,danielraffel/Shipyard,danielraffel/tartci,danielraffel/fleet-handbook", args)
        self.assertIn("--e2e-cache", args)

    def test_release_notify_runs_detached_in_tick_mode_and_keeps_its_stderr(self):
        fake = self.home / "fake_release_notify.py"
        fake.write_text(
            "import os, sys\n"
            "open(os.path.join(os.environ['HOME'], 'rn-args'), 'w').write(' '.join(sys.argv[1:]))\n"
            "sys.stderr.write('boom from release notify\\n')\n"
        )
        result = self.run_tick(SHIPYARD_RELEASE_NOTIFY_SCRIPT=str(fake))
        self.assertEqual(result.returncode, 0, result.stderr)
        err = self.log_dir / "release-notify.err"
        for _ in range(100):
            if err.exists() and "boom" in err.read_text():
                break
            time.sleep(0.1)
        self.assertIn("boom from release notify", err.read_text())
        self.assertEqual((self.home / "rn-args").read_text(), f"--tick --log-dir {self.log_dir}")

    def test_release_notify_off_switch(self):
        fake = self.home / "fake_release_notify.py"
        fake.write_text("import os\nopen(os.path.join(os.environ['HOME'], 'rn-args'), 'w').write('ran')\n")
        result = self.run_tick(SHIPYARD_RELEASE_NOTIFY_SCRIPT=str(fake), SHIPYARD_RELEASE_NOTIFY="off")
        self.assertEqual(result.returncode, 0, result.stderr)
        time.sleep(1)
        self.assertFalse((self.home / "rn-args").exists())

    def test_release_canary_off_switch(self):
        result = self.run_tick(SHIPYARD_RELEASE_CANARY="off")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.home / "canary-args").exists())

    def test_archive_failure_keeps_files_and_exit_status(self):
        self.run_tick()
        old = self.add_old_tick()
        archive_path = self.log_dir / "archive"
        archive_path.rmdir()
        archive_path.write_text("blocking directory creation\n")
        result = self.run_tick()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(old.exists())
        self.assertIn("evidence archive failed", (self.log_dir / "archive.log").read_text())
        self.assertNotIn("evidence archive failed", (self.log_dir / "summary.log").read_text())

    def test_day_still_inside_the_live_window_is_not_archived(self):
        self.run_tick()
        # Older than the 3-day age cut-off, but its UTC day has not fully
        # left the live window, so the day is not bundled yet.
        edge = self.add_old_tick("edge.json", days=3.2)
        result = self.run_tick()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(edge.exists())
        self.assertEqual(list((self.log_dir / "archive").glob("ticks-*.tar.gz")), [])

    def test_rerun_rebuilds_day_archive_without_losing_members(self):
        self.run_tick()
        first = self.add_old_tick("first.json")
        self.assertEqual(self.run_tick().returncode, 0)
        second = self.add_old_tick("second.json")
        self.assertEqual(self.run_tick().returncode, 0)
        self.assertFalse(first.exists())
        self.assertFalse(second.exists())
        archives = list((self.log_dir / "archive").glob("ticks-*.tar.gz"))
        self.assertEqual(len(archives), 1)
        with tarfile.open(archives[0], "r:gz") as archive:
            names = set(archive.getnames())
            self.assertIn("./first.json", names)
            self.assertIn("./second.json", names)

    def test_later_day_failure_keeps_earlier_day_files(self):
        self.run_tick()
        first = self.add_old_tick("first.json", days=6)
        second = self.add_old_tick("second.json", days=5)
        tar_state = self.home / "tar-calls"
        (self.home / ".local" / "bin" / "tar").write_text(
            "#!/bin/bash\n"
            "if [ \"$1\" = \"-czf\" ]; then\n"
            "  count=0\n"
            "  [ -f \"$TAR_FAIL_STATE\" ] && count=$(cat \"$TAR_FAIL_STATE\")\n"
            "  count=$((count + 1))\n"
            "  printf '%s' \"$count\" >\"$TAR_FAIL_STATE\"\n"
            "  [ \"$count\" -eq 2 ] && exit 1\n"
            "fi\n"
            "exec /usr/bin/tar \"$@\"\n"
        )
        (self.home / ".local" / "bin" / "tar").chmod(0o755)
        result = self.run_tick(TAR_FAIL_STATE=str(tar_state))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(first.exists())
        self.assertTrue(second.exists())
        self.assertIn("day=", (self.log_dir / "archive.log").read_text())

    def test_a_slow_deploy_does_not_hold_the_tick(self):
        libexec = self.home / ".local" / "libexec"
        libexec.mkdir(parents=True)
        marker = self.home / "deployed"
        deploy = libexec / "shipyard-fleet-deploy.sh"
        deploy.write_text(f"#!/bin/bash\nsleep 6\necho \"$FLEET_DEPLOY_LOG\" > {marker}\n")
        deploy.chmod(0o755)
        started = time.monotonic()
        result = self.run_tick()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLess(time.monotonic() - started, 5)
        self.assertFalse(marker.exists())
        for _ in range(60):
            if marker.exists():
                break
            time.sleep(0.25)
        self.assertTrue(marker.exists(), "the detached deploy never ran")
        self.assertEqual(marker.read_text().strip(), str(self.log_dir / "summary.log"))

    def steward_prints(self, prs, rc):
        import json
        body = json.dumps({"repos": [{"repo": "acme/repo", "prs": prs, "errors": []}]})
        (self.home / "pass.json").write_text(body)
        stub = self.home / ".local" / "bin" / "shipyard"
        stub.write_text(
            "#!/bin/bash\n"
            "if [ \"$1\" = --version ]; then echo 'shipyard 1.0'; exit 0; fi\n"
            f"cat '{self.home / 'pass.json'}'\nexit {rc}\n"
        )
        stub.chmod(0o755)

    def summary_line(self):
        lines = [l for l in (self.log_dir / "summary.log").read_text().splitlines() if "acme/repo" in l]
        self.assertTrue(lines, "no summary line")
        return lines[-1]

    POLICY = {"number": 11, "mutation": "none", "error": "needs an exact-head approval"}
    READ = {"number": 12, "mutation": "none",
            "error": "queue-arm-guard: cannot read live PR state: gh: HTTP 503 refusing to land it unreviewed"}

    def test_a_read_error_is_read_deferred_not_a_refusal(self):
        self.steward_prints([self.POLICY, self.READ], rc=1)
        self.run_tick()
        line = self.summary_line()
        self.assertIn("pr_refused=#11 read_deferred=#12", line)
        self.assertIn("rc=1", line)

    def test_only_a_read_error_is_not_a_failed_tick(self):
        self.steward_prints([self.READ], rc=1)
        self.run_tick()
        line = self.summary_line()
        self.assertIn("pr_refused=0 read_deferred=#12", line)
        self.assertIn("rc=0", line)

    def test_the_pass_may_name_the_error_kind(self):
        self.steward_prints([{"number": 5, "error": "odd text", "error_kind": "read"},
                             {"number": 6, "error": "HTTP 503 in a policy text", "error_kind": "policy"}], rc=1)
        self.run_tick()
        self.assertIn("pr_refused=#6 read_deferred=#5", self.summary_line())

    def steward_prints_invariants(self, invariants):
        import json
        body = json.dumps({"repos": [{"repo": "acme/repo", "prs": [], "errors": [], "invariants": invariants}]})
        (self.home / "pass.json").write_text(body)
        stub = self.home / ".local" / "bin" / "shipyard"
        stub.write_text(
            "#!/bin/bash\n"
            "if [ \"$1\" = --version ]; then echo 'shipyard 1.0'; exit 0; fi\n"
            f"cat '{self.home / 'pass.json'}'\n"
        )
        stub.chmod(0o755)

    def test_a_failing_invariant_with_a_fix_hint_renders_as_one_token(self):
        self.steward_prints_invariants([
            {"id": "required_context_producer", "ok": False, "detail": "d",
             "fix_hint": "fix #1382 ships in v0.433.0; host on v0.432.0",
             "fix_hint_token": "fix#1382@v0.433.0,host=v0.432.0,eta=03:40Z"},
            {"id": "required_never_ran", "ok": False, "detail": "d"},
            {"id": "approval_admission_config", "ok": True, "detail": "d"},
        ])
        self.run_tick()
        self.assertIn(
            " invariants=FAIL:required_context_producer(fix#1382@v0.433.0,host=v0.432.0,eta=03:40Z),required_never_ran red_everywhere=",
            self.summary_line(),
        )

    def test_a_spaced_hint_token_is_compacted(self):
        self.steward_prints_invariants([
            {"id": "required_context_producer", "ok": False, "detail": "d", "fix_hint_token": "a b;c"}])
        self.run_tick()
        self.assertIn(" invariants=FAIL:required_context_producer(a_b_c) ", self.summary_line())

    def test_advisory_checks_merged_over_are_one_token_per_pr(self):
        self.steward_prints([
            {"number": 69, "mutation": "merged_exact_head:abc,advisory_not_green:zizmor_(report_only)|lint"},
            {"number": 70, "mutation": "merged_exact_head:def"},
            {"number": 71, "mutation": "merged_exact_head:fed,advisory_not_green:a b\nc=d;e"},
        ], rc=0)
        self.run_tick()
        line = self.summary_line()
        self.assertTrue(line.endswith(
            " advisory_not_green=#69:zizmor_(report_only)|lint,#71:a_b_c_d_e"), line)
        self.assertNotIn("#70:", line)
        self.assertIn("enqueued=3 ", line)

    def test_no_advisory_field_without_an_advisory_merge(self):
        self.steward_prints([{"number": 1, "mutation": "merged_exact_head:abc"}], rc=0)
        self.run_tick()
        self.assertNotIn("advisory_not_green", self.summary_line())

    def test_clean_pass_reports_zero_for_both(self):
        self.steward_prints([{"number": 1, "mutation": "enqueued"}], rc=0)
        self.run_tick()
        self.assertIn("pr_refused=0 read_deferred=0", self.summary_line())

    def test_old_archives_are_pruned(self):
        self.run_tick()
        archive_dir = self.log_dir / "archive"
        old_archive = archive_dir / "ticks-20000101.tar.gz"
        with tarfile.open(old_archive, "w:gz"):
            pass
        old = time.time() - 3 * 86400
        os.utime(old_archive, (old, old))
        result = self.run_tick(SHIPYARD_STEWARD_ARCHIVE_RETENTION_DAYS="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(old_archive.exists())


FAKE_LAUNCHER = """#!/bin/bash
# Stand-in for ShipyardLauncher.app's executable.
# Shipyard arguments (a detached release rehearsal): exec keeps this pid, so
# the recorder reads this process's own responsibility.
if [ "$1" = release-canary ]; then
  exec /usr/bin/python3 "$HOME/canary-recorder.py" "$@"
fi
echo "$*" >> "$HOME/launcher-calls"
if [ "$1" = --launcher-active ]; then
  [ -n "${FAKE_ACTIVE_HANG:-}" ] && sleep 30
  # Answer active, then make the exec that follows fail.
  [ -n "${FAKE_EXEC_FAILS:-}" ] && { chmod a-x "$0"; echo '{"active":true,"reason":"grant_cached"}'; exit 0; }
  [ "${FAKE_VERIFY_RC:-0}" = 0 ] || { echo '{"active":false,"reason":"grant_revoked: demoted"}'; exit "$FAKE_VERIFY_RC"; }
  [ -n "${FAKE_ACTIVE_NO_VERDICT:-}" ] && { echo '{"reason":"grant_cached"}'; exit 0; }
  [ -n "${FAKE_PROBE_DEADLINE:-}" ] && { echo '{"active":false,"reason":"grant_unknown: no access report within 20s"}'; exit 1; }
  if [ -n "${FAKE_UNKNOWN_TIMES:-}" ]; then
    n=$(grep -c -- --launcher-active "$HOME/launcher-calls")
    [ "$n" -le "$FAKE_UNKNOWN_TIMES" ] && { echo '{"active":false,"reason":"grant_unknown: errno=4"}'; exit 1; }
  fi
  echo '{"active":true,"reason":"grant_cached"}'; exit 0
fi
if [ "$1" = --launcher-script ] && [ "$2" = steward-tick ]; then
  SHIPYARD_LAUNCHER_APP=/fake/ShipyardLauncher.app exec /bin/bash --noprofile --norc "$HOME/.local/libexec/shipyard-steward-tick.sh"
fi
exit 64
"""


class StewardTickLauncherTests(unittest.TestCase):
    """The tick routes itself through ShipyardLauncher.app once activated."""

    run_tick = StewardTickArchiveTests.run_tick
    tearDown = StewardTickArchiveTests.tearDown

    def setUp(self):
        StewardTickArchiveTests.setUp(self)
        libexec = self.home / ".local" / "libexec"
        libexec.mkdir(parents=True)
        self.installed = libexec / "shipyard-steward-tick.sh"
        self.installed.write_text(SCRIPT.read_text())
        self.installed.chmod(0o755)
        exe = libexec / "ShipyardLauncher.app" / "Contents" / "MacOS" / "shipyard-launcher"
        exe.parent.mkdir(parents=True)
        exe.write_text(FAKE_LAUNCHER)
        exe.chmod(0o755)
        self.record = self.home / ".local" / "share" / "shipyard" / "launcher" / "activation.json"
        self.calls = self.home / "launcher-calls"

    def activate(self):
        self.record.parent.mkdir(parents=True, exist_ok=True)
        self.record.write_text('{"schema": 1}\n')

    def run_installed(self, **env):
        current = self.env.copy()
        current.update(env)
        return subprocess.run(["/bin/bash", str(self.installed)], env=current, text=True, capture_output=True)

    def summary(self):
        return (self.log_dir / "summary.log").read_text()

    def test_not_activated_runs_directly_without_calling_the_launcher(self):
        result = self.run_installed()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("launcher=direct", self.summary())
        self.assertFalse(self.calls.exists())

    def test_activated_tick_runs_under_the_launcher(self):
        self.activate()
        result = self.run_installed()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls.read_text().splitlines(), ["--launcher-active", "--launcher-script steward-tick"])
        self.assertIn("acme/repo rc=0", self.summary())
        self.assertIn("launcher=app", self.summary())
        self.assertNotIn("launcher=direct", self.summary())

    def test_launcher_refusal_falls_back_and_says_why(self):
        self.activate()
        result = self.run_installed(FAKE_VERIFY_RC="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls.read_text().splitlines(), ["--launcher-active"])
        self.assertIn("launcher=direct reason=not_active rc=1: grant_revoked: demoted", self.summary())
        self.assertIn("acme/repo rc=0", self.summary())

    def test_exit_zero_without_an_active_verdict_runs_directly(self):
        self.activate()
        result = self.run_installed(FAKE_ACTIVE_NO_VERDICT="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls.read_text().splitlines(), ["--launcher-active"])
        self.assertIn("launcher=direct reason=not_active rc=0: grant_cached", self.summary())

    def test_a_hung_launcher_cannot_stall_the_tick(self):
        self.activate()
        started = time.monotonic()
        result = self.run_installed(FAKE_ACTIVE_HANG="1", SHIPYARD_STEWARD_LAUNCHER_TIMEOUT_S="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLess(time.monotonic() - started, 20)
        self.assertIn("launcher=direct reason=not_active rc=124: timed out after 1s", self.summary())
        self.assertIn("acme/repo rc=0", self.summary())

    def test_a_failed_exec_runs_the_tick_directly(self):
        self.activate()
        result = self.run_installed(FAKE_EXEC_FAILS="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("launcher=direct reason=not_active rc=exec_failed", self.summary())
        self.assertIn("acme/repo rc=0", self.summary())
        self.assertIn("launcher=direct", self.summary().splitlines()[-1])

    def test_a_launcher_fallback_never_routes_again(self):
        self.activate()
        result = self.run_installed(SHIPYARD_LAUNCHER_FALLBACK="launcher-script-mismatch")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.calls.exists())
        self.assertIn("launcher=direct reason=launcher-script-mismatch", self.summary())
        self.assertIn("launcher=direct invariants", self.summary().replace("launcher=direct reason", "x"))

    def test_only_the_installed_copy_routes(self):
        self.activate()
        result = self.run_tick()  # the repository copy, not ~/.local/libexec
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.calls.exists())
        self.assertIn("launcher=direct", self.summary())

    def findings(self):
        return [line for line in self.summary().splitlines() if " launcher-fallback " in line]

    def test_an_unknown_grant_is_asked_again_before_falling_back(self):
        self.activate()
        result = self.run_installed(FAKE_UNKNOWN_TIMES="1", SHIPYARD_STEWARD_LAUNCHER_RETRY_S="0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls.read_text().splitlines(),
                         ["--launcher-active", "--launcher-active", "--launcher-script steward-tick"])
        self.assertIn("launcher=app", self.summary())
        self.assertEqual(self.findings(), [])

    def test_an_unknown_grant_twice_falls_back_loudly(self):
        self.activate()
        result = self.run_installed(FAKE_UNKNOWN_TIMES="2", SHIPYARD_STEWARD_LAUNCHER_RETRY_S="0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls.read_text().splitlines(), ["--launcher-active", "--launcher-active"])
        self.assertIn("launcher=direct reason=not_active rc=1: grant_unknown: errno=4", self.summary())
        [finding] = self.findings()
        self.assertRegex(finding, r"^\d{8}T\d{6}Z launcher-fallback reason=grant_unknown FINDING detail=rc=1_grant_unknown:_errno=4$")
        self.assertIn("acme/repo rc=0", self.summary())

    def test_other_refusals_are_not_retried_but_are_loud(self):
        self.activate()
        self.run_installed(FAKE_VERIFY_RC="1")
        self.assertEqual(self.calls.read_text().splitlines(), ["--launcher-active"])
        [finding] = self.findings()
        self.assertIn("launcher-fallback reason=grant_revoked FINDING", finding)

    def test_every_fallback_with_an_activation_record_is_loud(self):
        cases = {
            "timed_out": dict(FAKE_ACTIVE_HANG="1", SHIPYARD_STEWARD_LAUNCHER_TIMEOUT_S="1"),
            "exec_failed": dict(FAKE_EXEC_FAILS="1"),
            "grant_cached": dict(FAKE_ACTIVE_NO_VERDICT="1"),
            "launcher-script-mismatch": dict(SHIPYARD_LAUNCHER_FALLBACK="launcher-script-mismatch"),
        }
        for reason, env in cases.items():
            with self.subTest(reason=reason):
                self.tearDown()
                self.setUp()
                self.activate()
                self.run_installed(**env)
                [finding] = self.findings()
                self.assertIn(f"launcher-fallback reason={reason} FINDING", finding)

    def test_a_missing_launcher_with_a_record_is_loud(self):
        self.activate()
        (self.home / ".local" / "libexec" / "ShipyardLauncher.app" / "Contents" / "MacOS" / "shipyard-launcher").unlink()
        result = self.run_installed()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("launcher=direct reason=launcher_missing", self.summary())
        [finding] = self.findings()
        self.assertIn("launcher-fallback reason=launcher_missing FINDING", finding)

    def test_a_probe_that_hit_its_deadline_is_not_retried(self):
        self.activate()
        self.run_installed(FAKE_PROBE_DEADLINE="1", SHIPYARD_STEWARD_LAUNCHER_RETRY_S="0")
        self.assertEqual(self.calls.read_text().splitlines(), ["--launcher-active"])
        [finding] = self.findings()
        self.assertIn("launcher-fallback reason=grant_unknown FINDING", finding)

    def test_a_demoted_record_is_loud_on_every_tick(self):
        self.record.parent.mkdir(parents=True, exist_ok=True)
        demoted = self.record.with_name("activation.json.demoted-1763190000")
        demoted.write_text('{"schema": 2}\n')
        for _ in range(2):
            result = self.run_installed()
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.calls.exists())
        self.assertEqual(self.summary().count("launcher=direct reason=grant_revoked_demoted"), 2)
        findings = self.findings()
        self.assertEqual(len(findings), 2)
        self.assertIn("launcher-fallback reason=grant_revoked_demoted FINDING", findings[0])
        self.assertIn(demoted.name, findings[0])
        # Reactivated: the old demoted file no longer pages.
        self.activate()
        self.run_installed()
        self.assertEqual(len(self.findings()), 2)

    def test_no_finding_without_an_activation_record_or_when_off(self):
        self.run_installed()
        self.activate()
        self.run_installed(SHIPYARD_STEWARD_LAUNCHER="off")
        self.run_tick()  # the repository copy never routes
        self.assertEqual(self.findings(), [])

    def test_off_switch(self):
        self.activate()
        result = self.run_installed(SHIPYARD_STEWARD_LAUNCHER="off")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.calls.exists())


if __name__ == "__main__":
    unittest.main()


CANARY_RECORDER = """
import ctypes, json, os, sys
libc = ctypes.CDLL(None)
responsible = libc.responsibility_get_pid_responsible_for_pid
responsible.restype, responsible.argtypes = ctypes.c_int, [ctypes.c_int]
with open(os.path.join(os.environ["HOME"], "canary-launcher.json"), "w") as out:
    json.dump({"own_responsible": responsible(os.getpid()) == os.getpid(),
               "cwd": os.getcwd(), "argv": sys.argv[1:]}, out)
print("release-canary status=pass tag=v1.0.1 issue=closed #5")
"""


@unittest.skipUnless(sys.platform == "darwin", "macOS responsibility")
class StewardTickReleaseCanaryLauncherTests(unittest.TestCase):
    """A detached rehearsal keeps the launcher's privacy identity after the tick."""

    run_tick = StewardTickArchiveTests.run_tick
    tearDown = StewardTickArchiveTests.tearDown
    activate = StewardTickLauncherTests.activate
    run_installed = StewardTickLauncherTests.run_installed
    summary = StewardTickLauncherTests.summary

    def setUp(self):
        StewardTickLauncherTests.setUp(self)
        (self.home / "canary-recorder.py").write_text(CANARY_RECORDER)
        shipyard = self.home / ".local" / "bin" / "shipyard"
        shipyard.write_text(
            "#!/bin/bash\n"
            "if [ \"$1\" = --version ]; then echo 'shipyard 1.0'; exit 0; fi\n"
            "if [ \"$1\" = release-canary ]; then\n"
            "  [ \"$3\" = --help ] && exit 0\n"
            "  echo \"$*\" > \"$HOME/canary-direct\"\n"
            "  echo 'release-canary status=pass tag=v1.0.1'; exit 0\n"
            "fi\n"
            "printf '%s\\n' '{\"repos\":[]}'\n"
        )
        self.record_file = self.home / "canary-launcher.json"

    def wait_for(self, text):
        for _ in range(150):
            if text in self.summary():
                return
            time.sleep(0.1)
        self.fail(f"{text!r} never reached summary.log:\n{self.summary()}")

    def test_an_activated_tick_runs_the_rehearsal_through_the_launcher_disclaimed(self):
        self.activate()
        result = self.run_installed()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.wait_for("acme/repo release-canary status=pass")
        record = json.loads(self.record_file.read_text())
        self.assertTrue(record["own_responsible"], record)
        self.assertEqual(record["argv"], ["release-canary", "run", "--repo", "acme/repo", "--daily", "--apply"])
        self.assertEqual(os.path.realpath(record["cwd"]),
                         os.path.realpath(self.home / ".local" / "share" / "shipyard-steward" / "repo"))
        self.assertIn("release-canary status=pass tag=v1.0.1 issue=closed #5 launcher=app", self.summary())
        self.assertFalse((self.home / "canary-direct").exists())

    def test_a_tick_not_under_the_launcher_runs_the_rehearsal_directly(self):
        result = self.run_installed()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.wait_for("acme/repo release-canary status=pass")
        self.assertIn("release-canary status=pass tag=v1.0.1 launcher=direct", self.summary())
        self.assertFalse(self.record_file.exists())

    def test_without_the_disclaim_it_runs_directly_and_says_so(self):
        # A python3 whose ctypes cannot set the disclaim: the spawner names it
        # and runs nothing, so the rehearsal runs directly, once.
        self.activate()
        installed = self.installed.read_text()
        self.assertEqual(installed.count("responsibility_spawnattrs_setdisclaim(ctypes.byref(attr), 1)"), 1)
        self.installed.write_text(installed.replace(
            "responsibility_spawnattrs_setdisclaim(ctypes.byref(attr), 1)",
            "no_such_symbol_for_test(ctypes.byref(attr), 1)"))
        result = self.run_installed()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.wait_for("acme/repo release-canary status=pass")
        self.assertIn("launcher-fallback reason=disclaim_unavailable FINDING", self.summary())
        self.assertIn("release-canary status=pass tag=v1.0.1 launcher=direct", self.summary())
        self.assertFalse(self.record_file.exists())

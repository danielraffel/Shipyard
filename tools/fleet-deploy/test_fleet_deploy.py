"""Tests for tools/fleet-deploy/fleet-deploy.sh against local git remotes."""
import json
import os
import pathlib
import shutil
import subprocess
import time
import sys
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
SCRIPT = HERE / "fleet-deploy.sh"
CHECK = HERE.parent / "shipyard-launcher" / "shipyard-launcher-check"

FAKE_CODESIGN = """#!/bin/bash
# Stand-in for /usr/bin/codesign: -dv reports a team, --verify obeys the test.
case "$1" in
  -dv) echo "TeamIdentifier=TEAM123456" >&2; exit 0 ;;
  -dvvv) echo "TeamIdentifier=TEAM123456" >&2; echo "CDHash=0123456789abcdef0123456789abcdef01234567" >&2; exit 0 ;;
  --verify) [ -f "$FAKE_TOOLS/codesign-refuses" ] && { echo "invalid signature" >&2; exit 1; }; exit 0 ;;
esac
exit 2
"""
FAKE_OPEN = """#!/bin/bash
# Stand-in for /usr/bin/open answering the launcher's grant probe.
result="${@: -1}"
printf '{"schema":1,"full_disk_access":"%s","errno":0,"ppid":1}\\n' "${FAKE_FDA:-denied}" > "$result"
"""
FAKE_SPCTL = """#!/bin/bash
echo "$4: accepted"
echo "source=Notarized Developer ID"
"""

FAKE_INSTALLER = """#!/bin/bash
# Stand-in for fleet-handbook's install-pass-runner.sh.
[ -f "$(dirname "$0")/FAIL" ] && { echo "self-test failed" >&2; exit 3; }
echo "$(git rev-parse --short=12 HEAD)" >> "$FLEET_PASS_LIVE/installs"
"""


def git(cwd, *args):
    subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True)


class FleetDeployTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        t = pathlib.Path(self.tmp.name)
        self.t = t
        self.live = t / "live"
        self.live.mkdir()
        (t / "logs").mkdir()
        self.log = t / "logs" / "summary.log"
        self.shipyard = self.make_repo("Shipyard", {
            "tools/steward-tick/shipyard-steward-tick.sh": "#!/bin/bash\necho tick v1\n",
            "tools/fleet-deploy/fleet-deploy.sh": SCRIPT.read_text(),
            "tools/shipyard-launcher/shipyard-launcher-check": CHECK.read_text(),
        })
        self.handbook = self.make_repo("fleet-handbook", {
            "scripts/install-pass-runner.sh": FAKE_INSTALLER,
        })
        self.tick_dest = t / "libexec" / "shipyard-steward-tick.sh"
        self.self_dest = t / "libexec" / "shipyard-fleet-deploy.sh"
        self.env = os.environ.copy()
        self.env.update({
            "HOME": str(t),
            "FLEET_DEPLOY_ROOT": str(t / "deploy"),
            "FLEET_DEPLOY_LOG": str(self.log),
            "FLEET_DEPLOY_INTERVAL_S": "0",
            "FLEET_DEPLOY_SHIPYARD_REMOTE": str(self.shipyard),
            "FLEET_DEPLOY_HANDBOOK_REMOTE": str(self.handbook),
            "FLEET_DEPLOY_TICK_DEST": str(self.tick_dest),
            "FLEET_DEPLOY_SELF_DEST": str(self.self_dest),
            "FLEET_PASS_LIVE": str(self.live),
            "GIT_CONFIG_GLOBAL": str(t / "gitconfig"),
            "FAKE_TOOLS": str(t / "tools"),
            "SHIPYARD_LAUNCHER_CHECK_CODESIGN": str(t / "tools" / "codesign"),
            "SHIPYARD_LAUNCHER_CHECK_SPCTL": str(t / "tools" / "spctl"),
            "SHIPYARD_LAUNCHER_CHECK_OPEN": str(t / "tools" / "open"),
            "SHIPYARD_LAUNCHER_CHECK_TEST_MODE": "1",
            "SHIPYARD_LAUNCHER_CHECK_TEAM": "TEAM123456",
            "SHIPYARD_LAUNCHER_CHECK_HOME": str(t),
            "SHIPYARD_LAUNCHER_CHECK_WAIT_SECS": "3",
        })
        (t / "tools").mkdir()
        for name, text in (("codesign", FAKE_CODESIGN), ("spctl", FAKE_SPCTL), ("open", FAKE_OPEN)):
            (t / "tools" / name).write_text(text)
            (t / "tools" / name).chmod(0o755)
        (t / ".local" / "bin").mkdir(parents=True)
        (t / ".local" / "bin" / "shipyard").write_text("#!/bin/sh\n")
        self.staged = t / ".local" / "share" / "shipyard" / "launcher" / "staged" / "ShipyardLauncher.app"
        self.launcher_dest = t / ".local" / "libexec" / "ShipyardLauncher.app"
        (t / "gitconfig").write_text("[user]\n\tname = t\n\temail = t@example.invalid\n[init]\n\tdefaultBranch = main\n")

    def tearDown(self):
        self.tmp.cleanup()

    def make_repo(self, name, files):
        work = self.t / f"{name}-work"
        bare = self.t / f"{name}.git"
        work.mkdir()
        env = {"GIT_CONFIG_GLOBAL": str(self.t / "gitconfig")}
        (self.t / "gitconfig").write_text("[user]\n\tname = t\n\temail = t@example.invalid\n[init]\n\tdefaultBranch = main\n")
        os.environ.update(env)
        git(work, "init", "-q", "-b", "main")
        for path, text in files.items():
            p = work / path
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text)
        git(work, "add", "-A")
        git(work, "commit", "-q", "-m", "init")
        subprocess.run(["git", "clone", "-q", "--bare", str(work), str(bare)], check=True)
        git(work, "remote", "add", "origin", str(bare))
        return bare

    def commit(self, name, path, text):
        work = self.t / f"{name}-work"
        p = work / path
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)
        git(work, "add", "-A")
        git(work, "commit", "-q", "-m", "change")
        git(work, "push", "-q", "origin", "main")

    def run_deploy(self, **env):
        e = self.env.copy()
        e.update(env)
        return subprocess.run(["bash", str(SCRIPT)], env=e, capture_output=True, text=True)

    def installs(self):
        p = self.live / "installs"
        return p.read_text().split() if p.exists() else []

    def test_first_run_installs_every_unit(self):
        r = self.run_deploy()
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.tick_dest.read_text(), "#!/bin/bash\necho tick v1\n")
        self.assertTrue(os.access(self.tick_dest, os.X_OK))
        self.assertEqual(self.self_dest.read_text(), SCRIPT.read_text())
        self.assertEqual(len(self.installs()), 1)
        log = self.log.read_text()
        self.assertIn("unit=tick installed", log)
        self.assertIn("unit=deploy installed", log)
        self.assertIn("unit=pass-runner installed", log)

    def test_unchanged_main_installs_nothing(self):
        self.run_deploy()
        before = self.log.read_text()
        r = self.run_deploy()
        self.assertEqual(r.returncode, 0)
        self.assertEqual(self.log.read_text(), before)
        self.assertEqual(len(self.installs()), 1)

    def test_a_merged_change_is_deployed_with_a_backup(self):
        self.run_deploy()
        self.commit("Shipyard", "tools/steward-tick/shipyard-steward-tick.sh", "#!/bin/bash\necho tick v2\n")
        self.commit("fleet-handbook", "scripts/pass-runner/x.py", "print(1)\n")
        self.run_deploy()
        self.assertIn("tick v2", self.tick_dest.read_text())
        backups = list(self.tick_dest.parent.glob("shipyard-steward-tick.sh.bak-*"))
        self.assertEqual(len(backups), 1)
        self.assertIn("tick v1", backups[0].read_text())
        self.assertEqual(len(self.installs()), 2)

    def test_a_broken_script_on_main_is_refused_and_keeps_the_old_one(self):
        self.run_deploy()
        self.commit("Shipyard", "tools/steward-tick/shipyard-steward-tick.sh", "#!/bin/bash\nif then fi (\n")
        r = self.run_deploy()
        self.assertEqual(r.returncode, 0)
        self.assertIn("tick v1", self.tick_dest.read_text())
        self.assertIn("unit=tick refused", self.log.read_text())
        self.assertIn("reason=syntax", self.log.read_text())

    def test_a_failing_installer_is_retried_and_logged(self):
        self.commit("fleet-handbook", "scripts/FAIL", "1\n")
        r = self.run_deploy()
        self.assertEqual(r.returncode, 0)
        self.assertEqual(self.installs(), [])
        self.assertIn("unit=pass-runner refused", self.log.read_text())
        self.commit("fleet-handbook", "scripts/FAIL", "")
        os.remove(self.t / "fleet-handbook-work" / "scripts" / "FAIL")
        git(self.t / "fleet-handbook-work", "add", "-A")
        git(self.t / "fleet-handbook-work", "commit", "-q", "-m", "fix")
        git(self.t / "fleet-handbook-work", "push", "-q", "origin", "main")
        self.run_deploy()
        self.assertEqual(len(self.installs()), 1)

    def test_unreachable_remote_logs_and_exits_zero(self):
        r = self.run_deploy(FLEET_DEPLOY_SHIPYARD_REMOTE=str(self.t / "missing.git"))
        self.assertEqual(r.returncode, 0)
        self.assertIn("unit=tick refused reason=fetch_failed", self.log.read_text())
        self.assertFalse(self.tick_dest.exists())

    def test_off_switch_and_interval(self):
        r = self.run_deploy(SHIPYARD_FLEET_DEPLOY="off")
        self.assertEqual(r.returncode, 0)
        self.assertFalse(self.log.exists())
        self.run_deploy(FLEET_DEPLOY_INTERVAL_S="3600")
        self.commit("Shipyard", "tools/steward-tick/shipyard-steward-tick.sh", "#!/bin/bash\necho tick v3\n")
        self.run_deploy(FLEET_DEPLOY_INTERVAL_S="3600")
        self.assertIn("tick v1", self.tick_dest.read_text())

    # -- launcher unit (macOS only: it needs ditto) --------------------------

    def stage_launcher(self, version, bundle_version="1", active_body=None):
        info = self.staged / "Contents" / "Info.plist"
        info.parent.mkdir(parents=True, exist_ok=True)
        info.write_text(
            '<?xml version="1.0" encoding="UTF-8"?>\n<plist version="1.0"><dict>'
            f"<key>CFBundleVersion</key><string>{bundle_version}</string></dict></plist>\n"
        )
        exe = self.staged / "Contents" / "MacOS" / "shipyard-launcher"
        exe.parent.mkdir(parents=True, exist_ok=True)
        extra = f"    {active_body}\n" if active_body else ""
        exe.write_text(
            "#!/bin/sh\n"
            "# Stand-in for the launcher: active only once a record exists.\n"
            "case \"$1\" in\n"
            "  --launcher-active)\n"
            + extra
            + "    [ -f \"$HOME/.local/share/shipyard/launcher/activation.json\" ] && "
            "{ echo '{\"active\":true,\"reason\":\"grant_cached\"}'; exit 0; }\n"
            "    echo '{\"active\":false,\"reason\":\"not_activated\"}'; exit 1 ;;\n"
            "  --launcher-verify) echo '{\"ok\":true,\"target\":\"/x/shipyard\"}'; exit 0 ;;\n"
            "esac\n"
            f"echo {version}\n"
        )
        exe.chmod(0o755)

    @unittest.skipUnless(sys.platform == "darwin", "the launcher unit runs on macOS only")
    def test_launcher_absent_is_logged_once(self):
        self.run_deploy()
        self.run_deploy()
        self.assertEqual(self.log.read_text().count("unit=launcher absent"), 1)
        self.assertFalse(self.launcher_dest.exists())

    @unittest.skipUnless(sys.platform == "darwin", "the launcher unit runs on macOS only")
    def test_verified_launcher_is_installed_backed_up_and_never_activated(self):
        self.stage_launcher("v1")
        self.run_deploy()
        exe = self.launcher_dest / "Contents" / "MacOS" / "shipyard-launcher"
        self.assertIn("v1", exe.read_text())
        self.assertIn("unit=launcher installed", self.log.read_text())
        before = self.log.read_text()
        self.run_deploy()
        self.assertEqual(self.log.read_text(), before, "an unchanged bundle is not reinstalled")
        self.stage_launcher("v2")
        self.run_deploy()
        self.assertIn("v2", exe.read_text())
        backups = list(self.launcher_dest.parent.glob("ShipyardLauncher.app.bak-*"))
        self.assertEqual(len(backups), 1)
        self.assertIn("v1", (backups[0] / "Contents" / "MacOS" / "shipyard-launcher").read_text())
        self.assertFalse((self.t / ".local" / "share" / "shipyard" / "launcher" / "activation.json").exists())
        self.assertEqual(self.log.read_text().count("unit=launcher-activation waiting reason=full_disk_access=denied"), 1)

    @unittest.skipUnless(sys.platform == "darwin", "the launcher unit runs on macOS only")
    def test_launcher_is_activated_once_the_owner_granted_access(self):
        self.stage_launcher("v1")
        self.run_deploy()
        record = self.t / ".local" / "share" / "shipyard" / "launcher" / "activation.json"
        self.assertFalse(record.exists())
        self.run_deploy(FAKE_FDA="granted")
        self.assertTrue(record.exists())
        self.assertEqual(json.loads(record.read_text())["schema"], 2)
        self.assertIn("unit=launcher-activation activated", self.log.read_text())
        before = self.log.read_text()
        self.run_deploy(FAKE_FDA="granted")
        self.assertEqual(self.log.read_text(), before, "an active launcher is left alone")

    @unittest.skipUnless(sys.platform == "darwin", "the launcher unit runs on macOS only")
    def test_auto_activation_off_switch(self):
        self.stage_launcher("v1")
        self.run_deploy(FAKE_FDA="granted", SHIPYARD_LAUNCHER_AUTO_ACTIVATE="off")
        self.assertFalse((self.t / ".local" / "share" / "shipyard" / "launcher" / "activation.json").exists())
        self.assertNotIn("launcher-activation", self.log.read_text())

    @unittest.skipUnless(sys.platform == "darwin", "the launcher unit runs on macOS only")
    def test_unverified_launcher_is_refused_and_installed_one_kept(self):
        self.stage_launcher("v1")
        self.run_deploy()
        (self.t / "tools" / "codesign-refuses").write_text("")
        self.stage_launcher("v2-unsigned")
        self.run_deploy()
        self.run_deploy()
        exe = self.launcher_dest / "Contents" / "MacOS" / "shipyard-launcher"
        self.assertIn("v1", exe.read_text())
        log = self.log.read_text()
        self.assertEqual(log.count("unit=launcher refused"), 1, log)
        self.assertIn("signature does not satisfy", log)
        self.assertEqual(list(self.launcher_dest.parent.glob("*.tmp.*")), [])

    @unittest.skipUnless(sys.platform == "darwin", "the launcher unit runs on macOS only")
    def test_an_older_staged_launcher_is_never_installed_or_activated(self):
        self.stage_launcher("v2", bundle_version="2")
        self.run_deploy()
        exe = self.launcher_dest / "Contents" / "MacOS" / "shipyard-launcher"
        self.assertIn("v2", exe.read_text())
        self.stage_launcher("v1", bundle_version="1")
        self.run_deploy(FAKE_FDA="granted")
        self.assertIn("v2", exe.read_text())
        self.assertIn("unit=launcher refused rev=", self.log.read_text())
        self.assertIn("reason=downgrade staged=1 installed_floor=2", self.log.read_text())
        # A hand-copied older bundle at the install path is not activated either.
        shutil.rmtree(self.launcher_dest)
        subprocess.run(["ditto", str(self.staged), str(self.launcher_dest)], check=True)
        shutil.rmtree(self.staged)
        record = self.t / ".local" / "share" / "shipyard" / "launcher" / "activation.json"
        record.unlink(missing_ok=True)
        self.run_deploy(FAKE_FDA="granted")
        self.assertIn("unit=launcher-activation waiting reason=downgraded installed=1 floor=2", self.log.read_text())
        self.assertFalse((self.t / ".local" / "share" / "shipyard" / "launcher" / "activation.json").exists())

    @unittest.skipUnless(sys.platform == "darwin", "the launcher unit runs on macOS only")
    def test_a_hung_launcher_cannot_hold_the_deploy(self):
        self.stage_launcher("v1", active_body="sleep 30")
        started = time.monotonic()
        r = self.run_deploy(FLEET_DEPLOY_LAUNCHER_TIMEOUT_S="1", SHIPYARD_LAUNCHER_CHECK_TOOL_TIMEOUT_SECS="1")
        self.assertEqual(r.returncode, 0)
        self.assertLess(time.monotonic() - started, 25)
        self.assertFalse((self.t / ".local" / "share" / "shipyard" / "launcher" / "activation.json").exists())
        self.assertIn("unit=launcher-activation waiting", self.log.read_text())


if __name__ == "__main__":
    unittest.main()

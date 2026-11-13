"""Tests for tools/fleet-deploy/fleet-deploy.sh against local git remotes."""
import os
import pathlib
import subprocess
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
SCRIPT = HERE / "fleet-deploy.sh"

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
        })
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


if __name__ == "__main__":
    unittest.main()

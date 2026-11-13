import os
import subprocess
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


if __name__ == "__main__":
    unittest.main()

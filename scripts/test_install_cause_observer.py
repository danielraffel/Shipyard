from __future__ import annotations

import os
import plistlib
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
INSTALLER = ROOT / "scripts" / "install_cause_observer.sh"
TEMPLATE = ROOT / "launchd" / "com.danielraffel.shipyard.cause-observer.plist.template"


def executable(path: Path, body: str) -> Path:
    path.write_text("#!/bin/sh\n" + body, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return path


class InstallCauseObserverTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.home = self.root / "home"
        self.home.mkdir()
        self.calls = self.root / "launchctl-calls"
        self.shipyard = executable(
            self.root / "shipyard",
            'case "$*" in\n'
            '  "cause record --help") exit 0 ;;\n'
            "esac\nexit 2\n",
        )
        self.launchctl = executable(self.root / "launchctl", f'printf "%s\\n" "$*" >> "{self.calls}"\n')
        self.plutil = executable(self.root / "plutil", "exit 0\n")
        self.env = {
            "HOME": str(self.home),
            "PATH": "/usr/bin:/bin",
            "SHIPYARD_LAUNCHCTL_BIN": str(self.launchctl),
            "SHIPYARD_PLUTIL_BIN": str(self.plutil),
        }

    def tearDown(self) -> None:
        self.temp.cleanup()

    def run_installer(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["bash", str(INSTALLER), "--shipyard", str(self.shipyard), *args],
            env=self.env,
            text=True,
            capture_output=True,
            check=False,
        )

    def plist_path(self) -> Path:
        return self.home / "Library/LaunchAgents/com.danielraffel.shipyard.cause-observer.plist"

    def test_default_is_dry_run(self) -> None:
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("action=dry-run", result.stdout)
        self.assertFalse(self.plist_path().exists())
        self.assertFalse(self.calls.exists())

    def test_install_renders_exact_read_only_observer_argv(self) -> None:
        archive = self.home / "private-cause-archive"
        result = self.run_installer(
            "--repo", "Generous-Corp/pulp", "--workflow", "build.yml",
            "--dir", str(archive), "--base", "main", "--interval", "1800", "--install",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        document = plistlib.loads(self.plist_path().read_bytes())
        self.assertEqual(document["Label"], "com.danielraffel.shipyard.cause-observer")
        self.assertEqual(
            document["ProgramArguments"],
            [str(self.shipyard), "--cwd", str(self.home), "--json", "cause", "record", "--repo", "Generous-Corp/pulp",
             "--workflow", "build.yml", "--dir", str(archive), "--base", "main"],
        )
        self.assertEqual(document["StartInterval"], 1800)
        self.assertTrue(document["RunAtLoad"])
        self.assertEqual(document["EnvironmentVariables"]["HOME"], str(self.home))
        self.assertNotIn("@", self.plist_path().read_text())
        calls = self.calls.read_text().splitlines()
        uid = os.getuid()
        self.assertEqual(calls[0], f"bootout gui/{uid}/com.danielraffel.shipyard.cause-observer")
        self.assertEqual(calls[1], f"bootstrap gui/{uid} {self.plist_path()}")
        self.assertTrue(archive.is_dir())

    def test_invalid_path_or_interval_is_refused(self) -> None:
        for args in (("--dir", "relative"), ("--cwd", "relative"), ("--interval", "899"),
                     ("--repo", "pulp"), ("--repo", "bad repo"), ("--repo", "/repo"),
                     ("--repo", "owner/"), ("--repo", "owner/repo/extra")):
            with self.subTest(args=args):
                result = self.run_installer(*args)
                self.assertEqual(result.returncode, 2, result.stdout)
                self.assertFalse(self.plist_path().exists())

    def test_missing_cause_record_is_refused(self) -> None:
        executable(self.shipyard, "exit 2\n")
        result = self.run_installer("--install")
        self.assertEqual(result.returncode, 2)
        self.assertIn("has no 'cause record'", result.stderr)
        self.assertFalse(self.plist_path().exists())

    def test_template_is_valid_after_rendering(self) -> None:
        rendered = (
            TEMPLATE.read_text()
            .replace("@SHIPYARD@", "/x/shipyard")
            .replace("@REPO@", "o/r")
            .replace("@WORKFLOW@", "ci.yml")
            .replace("@DIR@", "/Users/ci/cause")
            .replace("@BASE@", "main")
            .replace("@CWD@", "/Users/ci/project")
            .replace("@HOME@", "/Users/ci")
            .replace("@INTERVAL@", "900")
            .replace("@LOG@", "/Users/ci/observer.log")
        )
        document = plistlib.loads(rendered.encode())
        self.assertIn("cause", document["ProgramArguments"])
        self.assertIn("record", document["ProgramArguments"])


if __name__ == "__main__":
    unittest.main()

from __future__ import annotations

import os
import plistlib
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
INSTALLER = ROOT / "scripts" / "install_fleet_reconcile.sh"
TEMPLATE = ROOT / "launchd" / "com.danielraffel.shipyard.fleet-reconcile.plist.template"


def executable(path: Path, body: str) -> Path:
    path.write_text("#!/bin/sh\n" + body, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return path


class InstallFleetReconcileTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.home = self.root / "home"
        self.home.mkdir()
        self.calls = self.root / "launchctl-calls"
        self.rehearsal_env = self.root / "rehearsal-env"
        self.rehearsal_exit = self.root / "rehearsal-exit"
        self.rehearsal_exit.write_text("0", encoding="utf-8")
        self.shipyard = executable(
            self.root / "shipyard",
            'case "$*" in\n'
            '  "runner fleet-reconcile --help") exit 0 ;;\n'
            f'  "--json runner fleet-reconcile "*) /usr/bin/env > "{self.rehearsal_env}"; '
            f'exit "$(cat "{self.rehearsal_exit}")" ;;\n'
            "esac\nexit 2\n",
        )
        self.launchctl = executable(
            self.root / "launchctl", f'printf "%s\\n" "$*" >> "{self.calls}"\n'
        )
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
        return self.home / "Library/LaunchAgents/com.danielraffel.shipyard.fleet-reconcile.plist"

    def test_default_is_a_dry_run_that_writes_and_loads_nothing(self) -> None:
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("action=dry-run", result.stdout)
        self.assertFalse(self.plist_path().exists())
        self.assertFalse(self.calls.exists())

    def test_install_renders_a_valid_agent_and_bootstraps_it(self) -> None:
        result = self.run_installer("--install")
        self.assertEqual(result.returncode, 0, result.stderr)
        document = plistlib.loads(self.plist_path().read_bytes())
        self.assertEqual(document["Label"], "com.danielraffel.shipyard.fleet-reconcile")
        self.assertEqual(
            document["ProgramArguments"],
            [str(self.shipyard), "--json", "runner", "fleet-reconcile",
             "--soak-minutes", "30", "--retry-hours", "6", "--apply"],
        )
        self.assertEqual(document["StartInterval"], 900)
        self.assertFalse(document["RunAtLoad"])
        self.assertEqual(document["EnvironmentVariables"]["HOME"], str(self.home))
        self.assertNotIn("@", self.plist_path().read_text())
        calls = self.calls.read_text().splitlines()
        uid = os.getuid()
        self.assertEqual(calls[0], f"bootout gui/{uid}/com.danielraffel.shipyard.fleet-reconcile")
        self.assertEqual(calls[1], f"bootstrap gui/{uid} {self.plist_path()}")

    def test_install_rehearses_under_the_agent_environment_first(self) -> None:
        result = self.run_installer("--install")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("rehearsal=ok (exit 0)", result.stdout)
        env = dict(
            line.split("=", 1) for line in self.rehearsal_env.read_text().splitlines() if "=" in line
        )
        self.assertEqual(env["HOME"], str(self.home))
        self.assertIn(f"{self.home}/.local/bin", env["PATH"])
        self.assertNotIn("SHIPYARD_LAUNCHCTL_BIN", env, "the rehearsal must not inherit the caller's env")

    def test_a_failing_rehearsal_refuses_to_load(self) -> None:
        for code in ("9", "4", "7"):
            with self.subTest(code=code):
                self.rehearsal_exit.write_text(code, encoding="utf-8")
                result = self.run_installer("--install")
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"exited {code}; not loading the agent", result.stderr)
                self.assertFalse(self.plist_path().exists())
                self.assertFalse(self.calls.exists())

    def test_rate_limited_terminal_or_busy_rehearsal_still_loads(self) -> None:
        for code in ("3", "5", "75"):
            with self.subTest(code=code):
                self.rehearsal_exit.write_text(code, encoding="utf-8")
                result = self.run_installer("--install")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertTrue(self.plist_path().exists())

    def test_dry_run_does_not_rehearse(self) -> None:
        self.run_installer()
        self.assertFalse(self.rehearsal_env.exists())

    def test_binary_without_fleet_reconcile_is_refused(self) -> None:
        executable(self.shipyard, "exit 2\n")
        result = self.run_installer("--install")
        self.assertEqual(result.returncode, 2)
        self.assertIn("has no 'runner fleet-reconcile'", result.stderr)
        self.assertFalse(self.plist_path().exists())

    def test_relative_binary_and_tiny_interval_are_refused(self) -> None:
        for args in (["--shipyard", "shipyard"], ["--interval", "30"]):
            with self.subTest(args=args):
                result = subprocess.run(
                    ["bash", str(INSTALLER), *(["--shipyard", str(self.shipyard)] if args[0] != "--shipyard" else []), *args],
                    env=self.env, text=True, capture_output=True, check=False,
                )
                self.assertEqual(result.returncode, 2, result.stdout)

    def test_template_is_a_valid_plist_once_rendered(self) -> None:
        rendered = (
            TEMPLATE.read_text()
            .replace("@SHIPYARD@", "/x/shipyard")
            .replace("@HOME@", "/Users/ci")
            .replace("@INTERVAL@", "900")
            .replace("@SOAK_MINUTES@", "30")
            .replace("@RETRY_HOURS@", "6")
        )
        document = plistlib.loads(rendered.encode())
        self.assertIn("--apply", document["ProgramArguments"])


if __name__ == "__main__":
    unittest.main()

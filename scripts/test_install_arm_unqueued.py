from __future__ import annotations

import plistlib
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
INSTALLER = ROOT / "scripts" / "install_arm_unqueued.sh"
TEMPLATE = ROOT / "launchd" / "com.danielraffel.shipyard.arm-unqueued.plist.template"
LABEL = "com.danielraffel.shipyard.arm-unqueued"


def executable(path: Path, body: str) -> Path:
    path.write_text("#!/bin/sh\n" + body, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return path


class InstallArmUnqueuedTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.home = self.root / "home"
        self.home.mkdir()
        self.calls = self.root / "launchctl-calls"
        self.rehearsal_args = self.root / "rehearsal-args"
        self.rehearsal_exit = self.root / "rehearsal-exit"
        self.rehearsal_exit.write_text("0", encoding="utf-8")
        # A Shipyard that advertises the flag in `runner steward --help` and
        # records the rehearsal argv it was asked to run.
        self.shipyard = executable(
            self.root / "shipyard",
            'case "$*" in\n'
            '  "runner steward --help") echo "      --arm-unqueued" ; exit 0 ;;\n'
            f'  "--json runner steward "*) printf "%s\\n" "$*" > "{self.rehearsal_args}"; '
            f'exit "$(cat "{self.rehearsal_exit}")" ;;\n'
            "esac\nexit 2\n",
        )
        # A Shipyard too old to know the flag at all.
        self.old_shipyard = executable(
            self.root / "shipyard-old",
            'case "$*" in\n'
            '  "runner steward --help") echo "      --apply" ; exit 0 ;;\n'
            "esac\nexit 2\n",
        )
        self.launchctl = executable(
            self.root / "launchctl", f'printf "%s\\n" "$*" >> "{self.calls}"\n'
        )
        self.env = {
            "HOME": str(self.home),
            "PATH": "/usr/bin:/bin",
            "SHIPYARD_LAUNCHCTL_BIN": str(self.launchctl),
            # The real plutil: the point of these tests is that the rendered
            # plist is valid, so a stub that always succeeds would prove nothing.
            "SHIPYARD_PLUTIL_BIN": "/usr/bin/plutil",
        }

    def tearDown(self) -> None:
        self.temp.cleanup()

    def run_installer(self, *args: str, shipyard: Path | None = None):
        return subprocess.run(
            [
                "bash",
                str(INSTALLER),
                "--shipyard",
                str(shipyard or self.shipyard),
                *args,
            ],
            env=self.env,
            text=True,
            capture_output=True,
            check=False,
        )

    def plist_path(self) -> Path:
        return self.home / "Library/LaunchAgents" / f"{LABEL}.plist"

    # -- refusals ---------------------------------------------------------

    def test_repo_is_required_because_launchd_has_no_working_directory(self) -> None:
        result = self.run_installer()
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("--repo is required", result.stderr)

    def test_a_repo_that_is_not_owner_slash_repo_is_refused(self) -> None:
        for bad in ["notaslug", "a/b/c", "/abs", "trailing/"]:
            result = self.run_installer("--repo", bad)
            self.assertEqual(result.returncode, 2, f"{bad}: {result.stdout}")
            self.assertIn("must be OWNER/REPO", result.stderr, bad)

    def test_a_tick_faster_than_five_minutes_is_refused(self) -> None:
        result = self.run_installer("--repo", "o/r", "--interval", "60")
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("at least 300 seconds", result.stderr)

    def test_a_shipyard_without_the_flag_is_refused(self) -> None:
        # Loading the agent against an older binary would run the steward's
        # much broader --apply pass instead of only arming.
        result = self.run_installer("--repo", "o/r", shipyard=self.old_shipyard)
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("--arm-unqueued", result.stderr)

    def test_a_failing_rehearsal_refuses_to_load_the_agent(self) -> None:
        self.rehearsal_exit.write_text("1", encoding="utf-8")
        result = self.run_installer("--repo", "o/r", "--install")
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertFalse(self.plist_path().exists())
        self.assertFalse(self.calls.exists())

    # -- dry run ----------------------------------------------------------

    def test_default_is_a_dry_run_that_writes_and_loads_nothing(self) -> None:
        result = self.run_installer("--repo", "o/r")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("action=dry-run", result.stdout)
        self.assertFalse(self.plist_path().exists())
        self.assertFalse(self.calls.exists())
        self.assertFalse(self.rehearsal_args.exists())

    # -- install ----------------------------------------------------------

    def test_install_renders_a_valid_plist_with_every_repo(self) -> None:
        result = self.run_installer(
            "--repo", "Owner/one", "--repo", "Owner/two", "--install"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        plist = self.plist_path()
        self.assertTrue(plist.exists(), result.stdout)
        # No unrendered placeholder survived. This is the regression that a
        # variable-based substitution caused: BSD awk rejects a newline in a
        # -v assignment, so the repo arguments silently failed to render.
        self.assertNotIn("@", plist.read_text(encoding="utf-8").replace("@HOME@", ""))
        with plist.open("rb") as stream:
            parsed = plistlib.load(stream)
        argv = parsed["ProgramArguments"]
        self.assertEqual(argv[0], str(self.shipyard))
        self.assertEqual(parsed["Label"], LABEL)
        self.assertEqual(parsed["StartInterval"], 900)
        for repo in ("Owner/one", "Owner/two"):
            self.assertEqual(argv.count(repo), 1, argv)
        self.assertEqual(argv.count("--repo"), 2, argv)

    def test_the_installed_agent_arms_and_does_nothing_else(self) -> None:
        result = self.run_installer("--repo", "o/r", "--install")
        self.assertEqual(result.returncode, 0, result.stderr)
        with self.plist_path().open("rb") as stream:
            argv = plistlib.load(stream)["ProgramArguments"]
        self.assertIn("--arm-unqueued", argv)
        self.assertIn("--apply", argv)
        # Arming only: run cancellation and capacity preemption are not an
        # unattended tick's business.
        self.assertIn("--no-coalesce", argv)
        self.assertIn("--no-preempt-capacity", argv)

    def test_the_rehearsal_runs_in_audit_mode(self) -> None:
        result = self.run_installer("--repo", "o/r", "--install")
        self.assertEqual(result.returncode, 0, result.stderr)
        rehearsal = self.rehearsal_args.read_text(encoding="utf-8")
        self.assertIn("--arm-unqueued", rehearsal)
        # A rehearsal that armed would mutate the very thing it is checking.
        self.assertNotIn("--apply", rehearsal)

    def test_install_reloads_the_agent_through_launchctl(self) -> None:
        result = self.run_installer("--repo", "o/r", "--install")
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls.read_text(encoding="utf-8")
        self.assertIn("bootout", calls)
        self.assertIn("bootstrap", calls)

    def test_a_custom_base_and_interval_reach_the_plist(self) -> None:
        result = self.run_installer(
            "--repo", "o/r", "--base", "develop/next", "--interval", "600", "--install"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        with self.plist_path().open("rb") as stream:
            parsed = plistlib.load(stream)
        self.assertEqual(parsed["StartInterval"], 600)
        argv = parsed["ProgramArguments"]
        self.assertEqual(argv[argv.index("--base") + 1], "develop/next")

    def test_the_template_declares_every_placeholder_the_installer_renders(self) -> None:
        template = TEMPLATE.read_text(encoding="utf-8")
        for placeholder in ("@SHIPYARD@", "@HOME@", "@BASE@", "@INTERVAL@", "@REPO_ARGS@"):
            self.assertIn(placeholder, template)


if __name__ == "__main__":
    unittest.main()

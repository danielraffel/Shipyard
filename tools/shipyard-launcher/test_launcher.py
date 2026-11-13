"""Runtime tests for the ShipyardLauncher.app executable (main.swift).

macOS only: each run compiles main.swift twice with xcrun swiftc.

- The test build (-D SHIPYARD_LAUNCHER_TESTING) takes its home directory and
  its signature requirement from the environment, so ad-hoc signed fixtures
  stand in for Developer ID signed releases. It covers target resolution,
  refusal, and argument, environment, exit status and signal pass-through.
- The production build is run unsigned and ad-hoc signed to prove that a
  launcher without a Developer ID team refuses to run anything.

What only a real Developer ID build can show (see README.md): that a release
signed by the same team is accepted under the real requirement, notarization,
and the TCC attribution itself.
"""

import json
import os
import pathlib
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
import unittest

HERE = pathlib.Path(__file__).resolve().parent
SOURCE = HERE / "main.swift"
FIXTURE = HERE / "fixtures" / "target.c"
GENERATION = "a" * 64
ON_MAC = sys.platform == "darwin" and shutil.which("xcrun") is not None


def compile_launcher(app: pathlib.Path, testing: bool) -> pathlib.Path:
    exe = app / "Contents" / "MacOS" / "shipyard-launcher"
    exe.parent.mkdir(parents=True)
    args = ["xcrun", "swiftc", "-O", "-target", "arm64-apple-macos13", "-framework", "Security"]
    if testing:
        args += ["-D", "SHIPYARD_LAUNCHER_TESTING"]
    subprocess.run([*args, str(SOURCE), "-o", str(exe)], check=True, capture_output=True)
    shutil.copy(HERE / "Info.plist", app / "Contents" / "Info.plist")
    return exe


@unittest.skipUnless(ON_MAC, "needs macOS with xcrun swiftc")
class LauncherTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.build = tempfile.TemporaryDirectory()
        root = pathlib.Path(os.path.realpath(cls.build.name))
        cls.app = root / "ShipyardLauncher.app"
        cls.launcher = compile_launcher(cls.app, testing=True)
        cls.prod_app = root / "prod" / "ShipyardLauncher.app"
        cls.prod_launcher = compile_launcher(cls.prod_app, testing=False)
        cls.fixture = root / "fixture"
        subprocess.run(["clang", "-O", str(FIXTURE), "-o", str(cls.fixture)], check=True, capture_output=True)

    @classmethod
    def tearDownClass(cls):
        cls.build.cleanup()

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.home = pathlib.Path(os.path.realpath(self.tmp.name)) / "home"
        (self.home / ".local" / "bin").mkdir(parents=True, mode=0o755)
        (self.home / "Library" / "Logs").mkdir(parents=True)
        self.home.chmod(0o700)
        self.env = {
            "PATH": "/usr/bin:/bin",
            "SHIPYARD_LAUNCHER_TEST_HOME": str(self.home),
            "SHIPYARD_LAUNCHER_TEST_REQUIREMENT": 'identifier "shipyard"',
        }

    def tearDown(self):
        self.tmp.cleanup()

    # -- helpers -------------------------------------------------------------

    def install_release(self, identifier="shipyard", generation=GENERATION) -> pathlib.Path:
        directory = self.home / ".local" / "share" / "shipyard" / "auth-generations" / generation
        directory.mkdir(parents=True, mode=0o700)
        binary = directory / "shipyard"
        shutil.copy(self.fixture, binary)
        binary.chmod(0o700)
        if identifier is None:
            subprocess.run(["codesign", "--remove-signature", str(binary)], check=True, capture_output=True)
        else:
            subprocess.run(["codesign", "--force", "-s", "-", "-i", identifier, str(binary)], check=True, capture_output=True)
        pointer = self.home / ".local" / "bin" / "shipyard"
        if pointer.is_symlink() or pointer.exists():
            pointer.unlink()
        pointer.symlink_to(binary)
        return binary

    def launch(self, *args, env=None, launcher=None, **kwargs):
        merged = dict(self.env)
        merged.update(env or {})
        return subprocess.run(
            [str(launcher or self.launcher), *args],
            env=merged, capture_output=True, text=True, stdin=subprocess.DEVNULL, timeout=30, **kwargs,
        )

    def log(self) -> str:
        path = self.home / "Library" / "Logs" / "shipyard-launcher.log"
        return path.read_text() if path.exists() else ""

    # -- resolution and refusal ---------------------------------------------

    def test_runs_the_release_the_pointer_names(self):
        binary = self.install_release()
        result = self.launch("--launcher-verify")
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["target"], str(binary))
        self.assertTrue(report["ok"])

    def test_regular_file_pointer_is_accepted(self):
        pointer = self.home / ".local" / "bin" / "shipyard"
        shutil.copy(self.fixture, pointer)
        subprocess.run(["codesign", "--force", "-s", "-", "-i", "shipyard", str(pointer)], check=True, capture_output=True)
        pointer.chmod(0o755)
        result = self.launch("exit", "0")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_missing_pointer_is_127(self):
        result = self.launch("exit", "0")
        self.assertEqual(result.returncode, 127)
        self.assertIn("does not exist", result.stderr)

    def test_unsigned_release_is_refused(self):
        self.install_release(identifier=None)
        result = self.launch("exit", "0")
        self.assertEqual(result.returncode, 77)
        self.assertIn("not a Developer ID signed Shipyard", result.stderr)
        self.assertIn("refused code=77", self.log())

    def test_wrong_identity_is_refused(self):
        # Stands in for a binary of another team: it fails the requirement.
        self.install_release(identifier="not-shipyard")
        result = self.launch("args", "x")
        self.assertEqual(result.returncode, 77)
        self.assertNotIn("ARG:x", result.stdout)

    def test_release_swapped_after_the_static_check_is_killed(self):
        binary = self.install_release()
        impostor = binary.parent / "impostor"
        shutil.copy(self.fixture, impostor)
        subprocess.run(["codesign", "--force", "-s", "-", "-i", "impostor", str(impostor)], check=True, capture_output=True)
        result = self.launch("args", "ran", env={"SHIPYARD_LAUNCHER_TEST_SWAP_IN": str(impostor)})
        self.assertEqual(result.returncode, 77, result.stderr)
        self.assertIn("is not the verified Shipyard; killed it", result.stderr)
        self.assertNotIn("ARG:ran", result.stdout)

    def test_pointer_outside_generations_is_refused(self):
        elsewhere = self.home / "elsewhere"
        elsewhere.mkdir(mode=0o700)
        binary = elsewhere / "shipyard"
        shutil.copy(self.fixture, binary)
        subprocess.run(["codesign", "--force", "-s", "-", "-i", "shipyard", str(binary)], check=True, capture_output=True)
        (self.home / ".local" / "bin" / "shipyard").symlink_to(binary)
        result = self.launch("exit", "0")
        self.assertEqual(result.returncode, 77)
        self.assertIn("points outside", result.stderr)

    def test_malformed_generation_name_is_refused(self):
        self.install_release(generation="not-a-generation")
        result = self.launch("exit", "0")
        self.assertEqual(result.returncode, 77)

    def test_group_writable_generation_is_refused(self):
        binary = self.install_release()
        binary.parent.chmod(0o770)
        result = self.launch("exit", "0")
        self.assertEqual(result.returncode, 77)
        self.assertIn("group- or world-writable", result.stderr)

    def test_writable_pointer_directory_is_refused(self):
        self.install_release()
        (self.home / ".local" / "bin").chmod(0o777)
        try:
            result = self.launch("exit", "0")
        finally:
            (self.home / ".local" / "bin").chmod(0o755)
        self.assertEqual(result.returncode, 77)

    def test_unknown_launcher_option_is_usage(self):
        self.install_release()
        result = self.launch("--launcher-bogus")
        self.assertEqual(result.returncode, 64)

    # -- pass-through --------------------------------------------------------

    def test_arguments_pass_through_verbatim(self):
        self.install_release()
        args = ["--json", "a b", "", "--launcher-verify", "ünïcode", "$HOME"]
        result = self.launch("args", *args)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [f"ARG:{a}" for a in args])

    def test_environment_passes_through_without_dyld(self):
        self.install_release()
        result = self.launch(
            "env", "KEEP_ME", "DYLD_SHIPYARD_TEST", "SHIPYARD_LAUNCHER_APP", "BASH_ENV",
            env={"KEEP_ME": "kept", "DYLD_SHIPYARD_TEST": "1", "BASH_ENV": "/tmp/x"},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = result.stdout.splitlines()
        self.assertIn("ENV:KEEP_ME=kept", lines)
        self.assertIn("ENV:DYLD_SHIPYARD_TEST=<unset>", lines)
        self.assertIn(f"ENV:SHIPYARD_LAUNCHER_APP={self.app}", lines)
        # Only scripts lose shell start-up variables; a release keeps them.
        self.assertIn("ENV:BASH_ENV=/tmp/x", lines)

    def test_exit_status_passes_through(self):
        self.install_release()
        for code in (0, 1, 3, 75, 255):
            with self.subTest(code=code):
                self.assertEqual(self.launch("exit", str(code)).returncode, code)

    def test_child_killed_by_signal_kills_the_launcher_with_it(self):
        self.install_release()
        result = self.launch("selfkill", str(int(signal.SIGTERM)))
        self.assertEqual(result.returncode, -signal.SIGTERM)

    def test_signals_are_forwarded(self):
        self.install_release()
        for number in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP, signal.SIGUSR1):
            with self.subTest(signal=number.name):
                report = pathlib.Path(self.tmp.name) / f"report-{number.name}"
                env = dict(self.env)
                process = subprocess.Popen(
                    [str(self.launcher), "wait", str(report)], env=env,
                    stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                )
                ready = pathlib.Path(f"{report}.ready")
                deadline = time.monotonic() + 15
                while not ready.exists() and time.monotonic() < deadline:
                    time.sleep(0.05)
                self.assertTrue(ready.exists(), "fixture never became ready")
                child = int(ready.read_text())
                self.assertNotEqual(child, process.pid, "the launcher must spawn, not exec")
                process.send_signal(number)
                process.communicate(timeout=15)
                self.assertEqual(report.read_text().strip(), f"SIG:{int(number)}")
                self.assertEqual(process.returncode, 40 + int(number))

    def test_launch_is_logged(self):
        binary = self.install_release()
        self.launch("exit", "0")
        self.assertIn(f"run mode=shipyard target={binary}", self.log())

    # -- scripts -------------------------------------------------------------

    def write_tick(self, body: str, mode=0o755) -> pathlib.Path:
        libexec = self.home / ".local" / "libexec"
        libexec.mkdir(parents=True, exist_ok=True, mode=0o755)
        script = libexec / "shipyard-steward-tick.sh"
        script.write_text(body)
        script.chmod(mode)
        return script

    def test_script_runs_with_arguments_and_marker(self):
        self.write_tick('#!/bin/bash\necho "args=$*"\necho "app=$SHIPYARD_LAUNCHER_APP"\necho "bash_env=${BASH_ENV:-unset}"\nexit 7\n')
        result = self.launch("--launcher-script", "steward-tick", "one", "two", env={"BASH_ENV": "/tmp/evil"})
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertIn("args=one two", result.stdout)
        self.assertIn(f"app={self.app}", result.stdout)
        self.assertIn("bash_env=unset", result.stdout)

    def test_script_outside_allowlist_is_refused(self):
        self.write_tick("#!/bin/bash\nexit 0\n")
        result = self.launch("--launcher-script", "anything")
        self.assertEqual(result.returncode, 77)

    def test_world_writable_script_is_refused(self):
        self.write_tick("#!/bin/bash\necho ran\n", mode=0o777)
        result = self.launch("--launcher-script", "steward-tick")
        self.assertEqual(result.returncode, 77)
        self.assertNotIn("ran", result.stdout)

    def test_symlinked_script_is_refused(self):
        real = self.home / "real-tick.sh"
        real.write_text("#!/bin/bash\necho ran\n")
        real.chmod(0o755)
        libexec = self.home / ".local" / "libexec"
        libexec.mkdir(parents=True, mode=0o755)
        (libexec / "shipyard-steward-tick.sh").symlink_to(real)
        result = self.launch("--launcher-script", "steward-tick")
        self.assertEqual(result.returncode, 77)
        self.assertNotIn("ran", result.stdout)

    # -- read-only access check ---------------------------------------------

    def check_access(self, database: pathlib.Path):
        result_file = pathlib.Path(self.tmp.name) / f"access-{len(list(pathlib.Path(self.tmp.name).glob('access-*')))}.json"
        result = self.launch("--launcher-check-access", "--launcher-result", str(result_file),
                             env={"SHIPYARD_LAUNCHER_TEST_TCC_DB": str(database)})
        self.assertEqual(stat.S_IMODE(result_file.stat().st_mode), 0o600)
        self.assertEqual(json.loads(result_file.read_text()), json.loads(result.stdout))
        return result.returncode, json.loads(result.stdout)["full_disk_access"]

    def test_check_access_reports_granted_denied_and_unknown(self):
        database = pathlib.Path(self.tmp.name) / "TCC.db"
        database.write_bytes(b"")
        self.assertEqual(self.check_access(database), (0, "granted"))
        database.chmod(0o000)
        self.assertEqual(self.check_access(database), (1, "denied"))
        self.assertEqual(self.check_access(pathlib.Path(self.tmp.name) / "absent.db"), (2, "unknown"))

    def test_check_access_needs_no_signature_or_release(self):
        # It must work before any release is installed and in an unsigned
        # build, and it never resolves or runs a target.
        result = self.launch("--launcher-check-access", launcher=self.prod_launcher)
        self.assertIn(result.returncode, (0, 1, 2))
        self.assertIn("full_disk_access", result.stdout)

    # -- production build ----------------------------------------------------

    def test_production_build_without_developer_id_refuses_everything(self):
        self.install_release()
        self.write_tick("#!/bin/bash\necho ran\n")
        for signed in (False, True):
            if signed:
                subprocess.run(
                    ["codesign", "--force", "-s", "-", "-i", "com.danielraffel.shipyard.launcher", str(self.prod_app)],
                    check=True, capture_output=True,
                )
            for args in (["exit", "0"], ["--launcher-verify"], ["--launcher-script", "steward-tick"]):
                with self.subTest(signed=signed, args=args):
                    result = self.launch(*args, launcher=self.prod_launcher)
                    self.assertEqual(result.returncode, 77, result.stderr)
                    self.assertNotIn("ran", result.stdout)

    def test_production_build_ignores_test_overrides(self):
        # The environment cannot lower the requirement of a production build.
        self.install_release()
        result = self.launch("--launcher-verify", launcher=self.prod_launcher)
        self.assertEqual(result.returncode, 77)
        self.assertIn("Developer ID", result.stderr)


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
from __future__ import annotations

import importlib
import os
import shutil
import sys
import tempfile
import time
import unittest
from contextlib import redirect_stdout
from io import StringIO
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import package_release  # noqa: E402
import release_build_guard as guard  # noqa: E402


def _stub_runner(calls: list[list[str]]):
    """Real `ps`, canned answers for the slow or host-specific tools."""

    def run(args, timeout, env):
        calls.append(list(args))
        if args[0] == "ps":
            return guard._run_diagnostic(args, timeout, env)
        if args[0] == "sample":
            Path(args[args.index("-file") + 1]).write_text("Call graph:\n  wait4\n", encoding="utf-8")
            return ""
        if args[0] == "security":
            return f"{' '.join(args[1:])}: ok\n"
        return f"{args[0]} output\n"

    return run


class BoundedBuildTests(unittest.TestCase):
    def test_module_imports_without_pwd_as_on_windows(self) -> None:
        try:
            with mock.patch.dict(sys.modules, {"pwd": None}):
                reloaded = importlib.reload(guard)
                self.assertEqual(reloaded._real_home(), os.environ.get("HOME", "/"))
        finally:
            importlib.reload(guard)

    @unittest.skipIf(shutil.which("ps") is None, "needs ps")
    def test_stalled_build_is_killed_after_writing_diagnostics(self) -> None:
        calls: list[list[str]] = []
        with tempfile.TemporaryDirectory() as temp:
            diag = Path(temp) / "diag"
            marker = Path(temp) / "child.pid"
            script = f"sleep 120 & echo $! > {marker}; echo compiling-shipyard; sleep 120"

            def diagnose(*args, **kwargs):
                return guard.capture_diagnostics(*args, run=_stub_runner(calls), **kwargs)

            started = time.monotonic()
            with redirect_stdout(StringIO()) as out, self.assertRaises(guard.BuildTimedOut) as raised:
                guard.run_bounded(["/bin/sh", "-c", script], cwd=Path(temp), timeout=1.0,
                                  diagnostics_dir=diag, diagnose=diagnose)
            self.assertLess(time.monotonic() - started, 30)
            self.assertNotEqual(raised.exception.code, 0)
            self.assertIn("stall-report.txt", str(raised.exception.code))
            report = (diag / "stall-report.txt").read_text(encoding="utf-8")
            child = int(marker.read_text(encoding="utf-8").strip())
            for section in ("process tree", "lsof", "sample", "last output lines", "default keychain"):
                self.assertIn(f"===== {section}", report)
            # The tree is the build's own processes, found by real ps.
            self.assertIn("sleep 120", report)
            self.assertIn(str(child), report)
            self.assertIn("compiling-shipyard", report)
            self.assertIn("show-keychain-info: ok", report)
            self.assertIn("wait4", report)
            self.assertIn("compiling-shipyard", (diag / "cargo-build.log").read_text(encoding="utf-8"))
            self.assertIn("compiling-shipyard", out.getvalue())
            self.assertTrue(any(c[0] == "lsof" and str(child) in c[-1] for c in calls))
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                try:
                    os.kill(child, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.05)
            else:
                os.kill(child, 9)
                self.fail("a build child outlived the time limit")

    def test_a_child_that_ignores_sigterm_is_still_killed(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            marker = Path(temp) / "child.pid"
            # The leader exits on SIGTERM; its child ignores SIGTERM.
            script = f"(trap '' TERM; exec sleep 120) & echo $! > {marker}; sleep 120"
            with redirect_stdout(StringIO()), self.assertRaises(guard.BuildTimedOut), \
                    mock.patch.object(guard, "KILL_GRACE_SECS", 0.5):
                guard.run_bounded(["/bin/sh", "-c", script], cwd=Path(temp), timeout=1.0,
                                  diagnostics_dir=Path(temp), diagnose=lambda *a, **k: Path(temp) / "r")
            child = int(marker.read_text(encoding="utf-8").strip())
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                try:
                    os.kill(child, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.05)
            else:
                os.kill(child, 9)
                self.fail("a SIGTERM-ignoring child outlived the build kill")

    def test_report_keeps_sections_collected_before_a_kill(self) -> None:
        def run(args, timeout, env):
            if args[0] == "ps":
                return "100 1 100 S 01:00 0.0 cargo build\n"
            raise KeyboardInterrupt  # the step is killed while lsof runs

        with tempfile.TemporaryDirectory() as temp, redirect_stdout(StringIO()):
            with self.assertRaises(KeyboardInterrupt):
                guard.capture_diagnostics(100, ["last cargo line\n"], Path(temp), run=run, redact=lambda t: t)
            report = (Path(temp) / "stall-report.txt").read_text(encoding="utf-8")
        self.assertIn("last cargo line", report)
        self.assertIn("cargo build", report)

    def test_signing_refuses_binaries_other_than_the_built_ones(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            binary, companion = root / "shipyard", root / "shipyard-workstream-provider"
            binary.write_bytes(b"built")
            companion.write_bytes(b"built companion")
            manifest = root / "manifest.sha256"
            with redirect_stdout(StringIO()):
                package_release.write_build_manifest(manifest, [binary, companion])
                package_release.verify_build_manifest(manifest, [binary, companion])
                binary.write_bytes(b"swapped")
                with self.assertRaises(SystemExit) as raised:
                    package_release.verify_build_manifest(manifest, [binary, companion])
                self.assertIn("refusing to sign", str(raised.exception.code))
                with self.assertRaises(SystemExit):
                    package_release.verify_build_manifest(root / "missing", [binary, companion])

    def test_skip_build_checks_the_manifest_before_anything_runs(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            binary, companion = root / "shipyard", root / "shipyard-workstream-provider"
            binary.write_bytes(b"a")
            companion.write_bytes(b"b")
            manifest = root / "manifest.sha256"
            manifest.write_text("0" * 64 + "  shipyard\n" + "0" * 64 + "  shipyard-workstream-provider\n")
            args = package_release.parse_args([
                "--target", "macos-arm64", "--skip-build", "--binary", str(binary),
                "--companion-binary", str(companion), "--tag", "v1.0.0", "--dist-dir", str(root / "dist")])
            with mock.patch.dict(os.environ, {package_release.BUILD_MANIFEST_ENV: str(manifest)}), \
                    mock.patch.object(package_release, "smoke_binary_pair") as smoke, \
                    redirect_stdout(StringIO()), self.assertRaises(SystemExit) as raised:
                package_release.package(args)
            smoke.assert_not_called()
            self.assertIn("refusing to sign", str(raised.exception.code))

    def test_failed_build_raises_build_failed_without_diagnostics(self) -> None:
        diagnose = mock.Mock()
        with tempfile.TemporaryDirectory() as temp, redirect_stdout(StringIO()):
            with self.assertRaises(guard.BuildFailed):
                guard.run_bounded(["/bin/sh", "-c", "echo boom; exit 3"], cwd=Path(temp), timeout=30,
                                  diagnostics_dir=Path(temp), diagnose=diagnose)
        diagnose.assert_not_called()

    def test_stdin_is_closed_and_output_is_timestamped_and_redacted(self) -> None:
        read_fd, write_fd = os.pipe()
        saved = os.dup(0)
        os.dup2(read_fd, 0)
        try:
            with tempfile.TemporaryDirectory() as temp, redirect_stdout(StringIO()) as out:
                guard.run_bounded(["/bin/sh", "-c", "read line || echo eof; echo s3cret-token"],
                                  cwd=Path(temp), timeout=10, diagnostics_dir=Path(temp),
                                  redact=lambda t: t.replace("s3cret-token", "<redacted>"))
                log = (Path(temp) / "cargo-build.log").read_text(encoding="utf-8")
        finally:
            os.dup2(saved, 0)
            for fd in (saved, read_fd, write_fd):
                os.close(fd)
        self.assertIn("eof", log)
        self.assertNotIn("s3cret-token", log + out.getvalue())
        self.assertRegex(log, r"^\[\d\d:\d\d:\d\dZ \+\s*\d+\.\ds\] eof", )

    def test_diagnostics_list_only_the_build_tree_and_redact(self) -> None:
        ps = "\n".join([
            "  100     1   100 S 01:00 0.0 cargo build --release",
            "  101   100   100 S 01:00 0.0 rustc --crate-name shipyard",
            "  102   101   100 S 01:00 0.0 cc -o build-script",
            "  103     1   100 S 01:00 0.0 orphaned-build-script",
            "  200     1   200 S 01:00 0.0 notarytool --password hunter2",
        ])

        def run(args, timeout, env):
            if args[0] == "ps":
                return ps
            return "hunter2 seen\n"

        with tempfile.TemporaryDirectory() as temp, redirect_stdout(StringIO()):
            path = guard.capture_diagnostics(100, ["last hunter2 line\n"], Path(temp), run=run,
                                             redact=lambda t: t.replace("hunter2", "<redacted>"))
            report = path.read_text(encoding="utf-8")
        self.assertIn("orphaned-build-script", report)
        self.assertIn("cc -o build-script", report)
        self.assertNotIn("notarytool", report)
        self.assertNotIn("hunter2", report)

    def test_sample_targets_prefer_leaves_then_root(self) -> None:
        rows = guard.parse_ps("100 1 100 x\n101 100 100 x\n102 101 100 x\n103 100 100 x\n")
        self.assertEqual(guard.sample_targets(guard.build_tree(rows, 100), 100), [102, 103, 100])
        self.assertEqual(len(guard.sample_targets(guard.build_tree(rows, 100), 100, limit=1)), 1)

    def test_network_env_defaults_do_not_override_the_operator(self) -> None:
        env = guard.build_env({"CARGO_NET_RETRY": "2", "PATH": "/bin"})
        self.assertEqual(env["CARGO_NET_RETRY"], "2")
        self.assertEqual(env["CARGO_HTTP_TIMEOUT"], "60")
        self.assertEqual(env["CARGO_HTTP_LOW_SPEED_LIMIT"], "10")
        self.assertEqual(env["CARGO_TERM_PROGRESS_WHEN"], "never")
        self.assertEqual(env["GIT_TERMINAL_PROMPT"], "0")

    def test_timeout_is_configurable_and_validated(self) -> None:
        self.assertEqual(guard.timeout_seconds({}), 20 * 60)
        self.assertEqual(guard.timeout_seconds({guard.TIMEOUT_ENV: "2.5"}), 150)
        for bad in ("0", "-1", "soon"):
            with self.assertRaises(SystemExit):
                guard.timeout_seconds({guard.TIMEOUT_ENV: bad})

    def test_release_build_runs_cargo_verbose_under_the_guard(self) -> None:
        with mock.patch.object(guard, "run_bounded") as bounded, \
                mock.patch.dict(os.environ, {guard.TIMEOUT_ENV: "7", guard.DIAGNOSTICS_ENV: "/tmp/diag-x"}):
            package_release.build_release(None)
        args = bounded.call_args.args[0]
        self.assertEqual(args[:3], ["cargo", "build", "-v"])
        self.assertIn("--locked", args)
        kwargs = bounded.call_args.kwargs
        self.assertEqual(kwargs["timeout"], 7 * 60)
        self.assertEqual(kwargs["diagnostics_dir"], Path("/tmp/diag-x"))
        self.assertEqual(kwargs["env"]["GIT_TERMINAL_PROMPT"], "0")

    def test_build_only_builds_records_hashes_and_packages_nothing(self) -> None:
        args = package_release.parse_args(["--target", "macos-arm64", "--build-only"])
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            manifest = root / "m.sha256"

            def fake_path(target, cargo_target, name=package_release.BIN_NAME):
                path = root / name
                path.write_bytes(name.encode())
                return path

            with mock.patch.object(package_release, "build_release") as build, \
                    mock.patch.object(package_release, "default_binary_path", side_effect=fake_path), \
                    mock.patch.dict(os.environ, {package_release.BUILD_MANIFEST_ENV: str(manifest)}), \
                    redirect_stdout(StringIO()):
                self.assertEqual(package_release.package(args), [])
            build.assert_called_once_with(None)
            recorded = manifest.read_text(encoding="utf-8")
        self.assertIn("  shipyard\n", recorded)
        self.assertIn("  shipyard-workstream-provider\n", recorded)
        bad = package_release.parse_args(["--target", "macos-arm64", "--build-only", "--skip-build"])
        with self.assertRaises(SystemExit):
            package_release.package(bad)


if __name__ == "__main__":
    unittest.main()

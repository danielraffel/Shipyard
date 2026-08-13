#!/usr/bin/env python3
from __future__ import annotations

import os
import stat
import subprocess
import tempfile
import unittest
from contextlib import redirect_stdout
from io import StringIO
from pathlib import Path
from unittest import mock

import package_release


class PackageReleaseTests(unittest.TestCase):
    def test_codesign_uses_explicit_ephemeral_keychain_when_configured(self) -> None:
        with mock.patch.dict(
            os.environ,
            {
                "SHIPYARD_SIGNING_IDENTITY": "identity",
                "SHIPYARD_SIGNING_KEYCHAIN": "/tmp/signing keychain.keychain-db",
            },
            clear=True,
        ), mock.patch.object(package_release, "run") as run:
            package_release.sign_binary(Path("/tmp/shipyard"))
            package_release.sign_dmg(Path("/tmp/shipyard.dmg"))
        for call in run.call_args_list:
            self.assertIn("--keychain", call.args[0])
            self.assertIn("/tmp/signing keychain.keychain-db", call.args[0])

    def test_artifact_filename_keeps_dev_safe_prefix(self) -> None:
        self.assertEqual(
            package_release.artifact_filename(
                "shipyard",
                package_release.TARGETS["macos-arm64"],
            ),
            "shipyard-macos-arm64",
        )
        self.assertEqual(
            package_release.artifact_filename(
                "shipyard",
                package_release.TARGETS["windows-x64"],
            ),
            "shipyard-windows-x64.exe",
        )

    def test_require_signing_env_reports_all_missing_values(self) -> None:
        with mock.patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(SystemExit) as ctx:
                package_release.require_signing_env(notarize=True)
        message = str(ctx.exception)
        self.assertIn("SHIPYARD_SIGNING_IDENTITY", message)
        self.assertIn("SHIPYARD_NOTARIZE_APPLE_ID", message)
        self.assertIn("SHIPYARD_NOTARIZE_TEAM_ID", message)
        self.assertIn("SHIPYARD_NOTARIZE_APP_PASSWORD", message)

    def test_run_redacts_secrets_from_command_failures(self) -> None:
        result = subprocess.CompletedProcess(
            args=["fake"],
            returncode=1,
            stdout="",
            stderr="stderr contains app-secret and temp-secret",
        )
        with mock.patch.dict(
            os.environ,
            {
                "SHIPYARD_NOTARIZE_APP_PASSWORD": "app-secret",
                "SHIPYARD_SIGNING_IDENTITY": "developer-id-secret",
            },
            clear=True,
        ), mock.patch.object(package_release.subprocess, "run", return_value=result):
            with self.assertRaises(package_release.CommandFailed) as ctx:
                package_release.run(
                    [
                        "xcrun",
                        "notarytool",
                        "submit",
                        "--password",
                        "app-secret",
                        "--sign",
                        "developer-id-secret",
                        "-p",
                        "temp-secret",
                    ],
                    capture=True,
                    redact_values=("temp-secret",),
                )

        message = str(ctx.exception)
        self.assertIn("--password <redacted>", message)
        self.assertIn("-p <redacted>", message)
        self.assertNotIn("app-secret", message)
        self.assertNotIn("developer-id-secret", message)
        self.assertNotIn("temp-secret", message)

    def test_write_checksums_replaces_existing_artifact_line(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            artifact = root / "shipyard-linux-x64"
            artifact.write_text("one", encoding="utf-8")
            checksums = package_release.write_checksums(root, artifact)

            artifact.write_text("two", encoding="utf-8")
            package_release.write_checksums(root, artifact)

            lines = checksums.read_text(encoding="utf-8").splitlines()
            self.assertEqual(len(lines), 1)
            self.assertTrue(lines[0].endswith("  shipyard-linux-x64"))

    def test_plain_packaging_copies_binary_and_writes_checksum(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            fake_binary = root / "shipyard"
            fake_binary.write_text("#!/bin/sh\necho 'shipyard 0.1.0'\n", encoding="utf-8")
            fake_binary.chmod(fake_binary.stat().st_mode | stat.S_IXUSR)

            args = package_release.parse_args(
                [
                    "--skip-build",
                    "--binary",
                    str(fake_binary),
                    "--target",
                    "linux-x64",
                    "--tag",
                    "v-test",
                    "--dist-dir",
                    str(root / "dist"),
                ]
            )
            with redirect_stdout(StringIO()):
                artifacts = package_release.package(args)

            artifact = root / "dist" / "v-test" / "shipyard-linux-x64"
            self.assertEqual(artifacts, [artifact])
            self.assertTrue(artifact.exists())
            self.assertTrue((root / "dist" / "v-test" / "checksums.sha256").exists())

    def test_ci_mode_softens_dmg_mount_failure(self) -> None:
        def fake_run(args: list[str], **_kwargs: object) -> str:
            if args[:2] == ["hdiutil", "attach"]:
                raise package_release.CommandFailed("mount failed")
            raise AssertionError(f"unexpected command: {args}")

        with mock.patch.object(package_release, "require_commands"), \
                mock.patch.object(package_release, "run", side_effect=fake_run):
            result = package_release.smoke_dmg(
                Path("fake.dmg"),
                "shipyard",
                ci_mode=True,
            )

        self.assertIn("DMG mount skipped in CI mode", result)

    def test_local_mode_keeps_dmg_mount_failure_fatal(self) -> None:
        def fake_run(args: list[str], **_kwargs: object) -> str:
            if args[:2] == ["hdiutil", "attach"]:
                raise package_release.CommandFailed("mount failed")
            raise AssertionError(f"unexpected command: {args}")

        with mock.patch.object(package_release, "require_commands"), \
                mock.patch.object(package_release, "run", side_effect=fake_run):
            with self.assertRaises(package_release.CommandFailed):
                package_release.smoke_dmg(
                    Path("fake.dmg"),
                    "shipyard",
                    ci_mode=False,
                )

    def test_notarize_uses_keychain_profile_for_long_running_submit(self) -> None:
        calls: list[list[str]] = []

        def fake_run(args: list[str], **_kwargs: object) -> str:
            calls.append(args)
            if args[:3] == ["xcrun", "notarytool", "submit"]:
                return "status: Accepted"
            return ""

        with mock.patch.dict(
            os.environ,
            {
                "SHIPYARD_NOTARIZE_APPLE_ID": "apple@example.com",
                "SHIPYARD_NOTARIZE_TEAM_ID": "TEAM123",
                "SHIPYARD_NOTARIZE_APP_PASSWORD": "app-secret",
            },
            clear=True,
        ), mock.patch.object(
            package_release,
            "create_notary_keychain",
            return_value=(Path("/tmp/notary.keychain-db"), "keychain-secret"),
        ), mock.patch.object(
            package_release,
            "delete_notary_keychain",
        ) as delete_keychain, mock.patch.object(
            package_release,
            "run",
            side_effect=fake_run,
        ):
            package_release.notarize_and_staple(Path("shipyard.dmg"))

        store = next(
            args for args in calls
            if args[:3] == ["xcrun", "notarytool", "store-credentials"]
        )
        submit = next(
            args for args in calls
            if args[:3] == ["xcrun", "notarytool", "submit"]
        )

        self.assertIn("--password", store)
        self.assertNotIn("--password", submit)
        self.assertIn("--keychain-profile", submit)
        self.assertIn("--keychain", submit)
        self.assertIn("--timeout", submit)
        self.assertIn(package_release.NOTARY_WAIT_TIMEOUT, submit)
        delete_keychain.assert_called_once_with(Path("/tmp/notary.keychain-db"))


if __name__ == "__main__":
    unittest.main()

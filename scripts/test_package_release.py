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
                "SHIPYARD_SIGNING_HOME": "/tmp/isolated signing home",
            },
            clear=True,
        ), mock.patch.object(package_release, "run") as run:
            package_release.sign_binary(Path("/tmp/shipyard"))
            package_release.sign_dmg(Path("/tmp/shipyard.dmg"))
        for call in run.call_args_list:
            self.assertIn("--keychain", call.args[0])
            self.assertIn("/tmp/signing keychain.keychain-db", call.args[0])
            self.assertEqual(call.kwargs["env"]["HOME"], "/tmp/isolated signing home")

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

    def test_require_signing_env_accepts_complete_api_key_mode(self) -> None:
        with mock.patch.dict(
            os.environ,
            {
                "SHIPYARD_SIGNING_IDENTITY": "identity",
                "SHIPYARD_NOTARIZE_KEY_PATH": "/tmp/AuthKey_TEST.p8",
                "SHIPYARD_NOTARIZE_KEY_ID": "KEY123",
                "SHIPYARD_NOTARIZE_ISSUER_ID": "issuer-uuid",
            },
            clear=True,
        ):
            package_release.require_signing_env(notarize=True)

    def test_incomplete_api_key_mode_fails_closed(self) -> None:
        with mock.patch.dict(
            os.environ,
            {
                "SHIPYARD_SIGNING_IDENTITY": "identity",
                "SHIPYARD_NOTARIZE_KEY_PATH": "/tmp/AuthKey_TEST.p8",
            },
            clear=True,
        ):
            with self.assertRaises(SystemExit) as ctx:
                package_release.require_signing_env(notarize=True)
        self.assertIn("SHIPYARD_NOTARIZE_KEY_ID", str(ctx.exception))
        self.assertIn("SHIPYARD_NOTARIZE_ISSUER_ID", str(ctx.exception))

    def test_release_paths_expand_home_without_a_shell(self) -> None:
        with mock.patch.dict(
            os.environ,
            {
                "HOME": "/tmp/release-home",
                "SHIPYARD_NOTARIZE_KEY_PATH": "$HOME/keys/AuthKey_TEST.p8",
            },
            clear=True,
        ):
            path = package_release.expanded_env_path("SHIPYARD_NOTARIZE_KEY_PATH")
        self.assertEqual(
            path,
            Path("/tmp/release-home/keys/AuthKey_TEST.p8").resolve(),
        )

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
            fake_companion = root / "shipyard-workstream-provider"
            fake_companion.write_text(
                "#!/bin/sh\necho 'shipyard-workstream-provider 0.1.0'\n",
                encoding="utf-8",
            )
            fake_companion.chmod(fake_companion.stat().st_mode | stat.S_IXUSR)

            args = package_release.parse_args(
                [
                    "--skip-build",
                    "--binary",
                    str(fake_binary),
                    "--companion-binary",
                    str(fake_companion),
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
            companion = (
                root
                / "dist"
                / "v-test"
                / "shipyard-workstream-provider-linux-x64"
            )
            self.assertEqual(artifacts, [artifact, companion])
            self.assertTrue(artifact.exists())
            self.assertTrue(companion.exists())
            checksums = root / "dist" / "v-test" / "checksums.sha256"
            self.assertEqual(len(checksums.read_text(encoding="utf-8").splitlines()), 2)

    def test_packaging_refuses_a_missing_companion_binary(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            fake_binary = root / "shipyard"
            fake_binary.write_text("#!/bin/sh\necho 'shipyard 0.1.0'\n", encoding="utf-8")
            fake_binary.chmod(0o755)
            args = package_release.parse_args(
                [
                    "--skip-build",
                    "--binary",
                    str(fake_binary),
                    "--companion-binary",
                    str(root / "missing-provider"),
                    "--target",
                    "linux-x64",
                ]
            )

            with self.assertRaisesRegex(SystemExit, "companion binary"):
                package_release.package(args)

    def test_signed_dmg_stages_and_signs_both_binaries(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            binary = root / "shipyard"
            companion = root / "shipyard-workstream-provider"
            for path, name in (
                (binary, "shipyard"),
                (companion, "shipyard-workstream-provider"),
            ):
                path.write_text(f"#!/bin/sh\necho '{name} 0.127.0'\n", encoding="utf-8")
                path.chmod(0o755)
            args = package_release.parse_args(
                [
                    "--skip-build",
                    "--binary",
                    str(binary),
                    "--companion-binary",
                    str(companion),
                    "--target",
                    "macos-arm64",
                    "--tag",
                    "v0.127.0",
                    "--dist-dir",
                    str(root / "dist"),
                    "--dmg",
                    "--sign-macos",
                ]
            )
            staged_names: set[str] = set()

            def fake_create_dmg(stage: Path, output: Path, **_kwargs: object) -> None:
                staged_names.update(path.name for path in stage.iterdir())
                output.write_text("dmg", encoding="utf-8")

            with mock.patch.object(package_release, "require_commands"), \
                    mock.patch.object(package_release, "require_signing_env"), \
                    mock.patch.object(package_release, "prepared_signing_keychain"), \
                    mock.patch.object(package_release, "verify_signing_probe"), \
                    mock.patch.object(package_release, "sign_binary") as sign_binary, \
                    mock.patch.object(package_release, "sign_dmg"), \
                    mock.patch.object(
                        package_release, "create_dmg", side_effect=fake_create_dmg
                    ), mock.patch.object(
                        package_release,
                        "smoke_dmg",
                        return_value="shipyard 0.127.0\nshipyard-workstream-provider 0.127.0",
                    ) as smoke_dmg, redirect_stdout(StringIO()):
                artifacts = package_release.package(args)

            self.assertEqual(
                staged_names,
                {"shipyard", "shipyard-workstream-provider"},
            )
            self.assertEqual(
                {call.args[0].name for call in sign_binary.call_args_list},
                {"shipyard", "shipyard-workstream-provider"},
            )
            smoke_dmg.assert_called_once_with(
                artifacts[0],
                ("shipyard", "shipyard-workstream-provider"),
                ci_mode=False,
            )

    def test_ci_mode_softens_dmg_mount_failure(self) -> None:
        def fake_run(args: list[str], **_kwargs: object) -> str:
            if args[:2] == ["hdiutil", "attach"]:
                raise package_release.CommandFailed("mount failed")
            raise AssertionError(f"unexpected command: {args}")

        with mock.patch.object(package_release, "require_commands"), \
                mock.patch.object(package_release, "run", side_effect=fake_run):
            result = package_release.smoke_dmg(
                Path("fake.dmg"),
                ("shipyard", "shipyard-workstream-provider"),
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
                    ("shipyard", "shipyard-workstream-provider"),
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

    def test_notarize_api_key_mode_uses_direct_credentials(self) -> None:
        calls: list[list[str]] = []

        def fake_run(args: list[str], **_kwargs: object) -> str:
            calls.append(args)
            if args[:3] == ["xcrun", "notarytool", "submit"]:
                return "status: Accepted"
            return ""

        with mock.patch.dict(
            os.environ,
            {
                "SHIPYARD_NOTARIZE_KEY_PATH": "/tmp/AuthKey_TEST.p8",
                "SHIPYARD_NOTARIZE_KEY_ID": "KEY123",
                "SHIPYARD_NOTARIZE_ISSUER_ID": "issuer-uuid",
            },
            clear=True,
        ), mock.patch.object(package_release, "run", side_effect=fake_run):
            package_release.notarize_and_staple(Path("shipyard.dmg"))

        submit = calls[0]
        self.assertEqual(submit[:3], ["xcrun", "notarytool", "submit"])
        self.assertIn("--key", submit)
        self.assertIn("--key-id", submit)
        self.assertIn("--issuer", submit)
        self.assertNotIn("--password", submit)
        self.assertNotIn("store-credentials", " ".join(" ".join(call) for call in calls))

    def test_signing_keychain_is_first_and_search_list_is_restored(self) -> None:
        calls: list[list[str]] = []
        with tempfile.TemporaryDirectory() as temp:
            keychain = Path(temp) / "pulp-signing.keychain-db"
            keychain.touch()

            def fake_run(args: list[str], **kwargs: object) -> str:
                calls.append(args)
                if args == ["security", "list-keychains", "-d", "user"]:
                    return '"/tmp/login.keychain-db"\n'
                return ""

            with mock.patch.dict(
                os.environ,
                {"SHIPYARD_SIGNING_KEYCHAIN": str(keychain)},
                clear=True,
            ), mock.patch.object(package_release, "run", side_effect=fake_run):
                with package_release.signing_keychain_first():
                    calls.append(["inside"])

        prefix = ["security", "list-keychains", "-d", "user", "-s"]
        set_calls = [call for call in calls if call[:5] == prefix]
        self.assertEqual(str(keychain.resolve()), set_calls[0][5])
        self.assertEqual(set_calls[-1], [*prefix, "/tmp/login.keychain-db"])

    def test_empty_search_list_fails_before_any_write(self) -> None:
        calls: list[list[str]] = []
        with tempfile.TemporaryDirectory() as temp:
            keychain = Path(temp) / "signing.keychain-db"
            keychain.touch()

            def fake_run(args: list[str], **_kwargs: object) -> str:
                calls.append(args)
                return ""

            with mock.patch.dict(
                os.environ,
                {"SHIPYARD_SIGNING_KEYCHAIN": str(keychain)},
                clear=True,
            ), mock.patch.object(package_release, "run", side_effect=fake_run):
                with self.assertRaises(SystemExit):
                    with package_release.signing_keychain_first():
                        pass

        self.assertFalse(any("-s" in call for call in calls))

    def test_disposable_keychain_imports_p12_with_full_partition_list(self) -> None:
        calls: list[list[str]] = []
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            p12 = root / "signing.p12"
            p12.touch()

            def fake_run(args: list[str], **_kwargs: object) -> str:
                calls.append(args)
                return ""

            with mock.patch.dict(
                os.environ,
                {
                    "SHIPYARD_SIGNING_P12": str(p12),
                    "SHIPYARD_SIGNING_P12_PASSWORD": "p12-secret",
                },
                clear=True,
            ), mock.patch.object(package_release, "run", side_effect=fake_run):
                keychain, _password = package_release.create_disposable_signing_keychain(root)

        imported = next(call for call in calls if call[:2] == ["security", "import"])
        partitions = next(
            call
            for call in calls
            if call[:2] == ["security", "set-key-partition-list"]
        )
        self.assertIn(str(keychain), imported)
        self.assertIn("/usr/bin/codesign", imported)
        self.assertIn("apple-tool:,apple:,codesign:", partitions)

    def test_partition_failure_never_enters_signing_body(self) -> None:
        entered = False
        with tempfile.TemporaryDirectory() as temp:
            p12 = Path(temp) / "signing.p12"
            p12.touch()

            def fake_run(args: list[str], **_kwargs: object) -> str:
                if args[:2] == ["security", "set-key-partition-list"]:
                    raise package_release.CommandFailed("partition setup failed")
                return ""

            with mock.patch.dict(
                os.environ,
                {
                    "SHIPYARD_SIGNING_P12": str(p12),
                    "SHIPYARD_SIGNING_P12_PASSWORD": "p12-secret",
                },
                clear=True,
            ), mock.patch.object(package_release, "run", side_effect=fake_run):
                with self.assertRaises(package_release.CommandFailed):
                    with package_release.prepared_signing_keychain():
                        entered = True

        self.assertFalse(entered)

    def test_explicit_keychain_without_p12_fails_before_codesign(self) -> None:
        with mock.patch.dict(
            os.environ,
            {
                "SHIPYARD_SIGNING_KEYCHAIN": "/tmp/persistent.keychain-db",
                "SHIPYARD_SIGNING_IDENTITY": "identity",
            },
            clear=True,
        ), mock.patch.object(package_release, "verify_signing_probe") as probe:
            with self.assertRaises(SystemExit) as ctx:
                with package_release.prepared_signing_keychain():
                    probe()

        self.assertIn("disposable", str(ctx.exception))
        probe.assert_not_called()

    def test_ci_prepared_keychain_does_not_require_local_p12(self) -> None:
        with mock.patch.dict(
            os.environ,
            {
                "SHIPYARD_SIGNING_KEYCHAIN": "/tmp/ci.keychain-db",
                "SHIPYARD_SIGNING_KEYCHAIN_READY": "1",
                "CI": "true",
            },
            clear=True,
        ), mock.patch.object(package_release, "signing_keychain_first") as prepared:
            with package_release.prepared_signing_keychain():
                pass
        prepared.assert_called_once_with()

    def test_local_ready_marker_cannot_bypass_disposable_keychain(self) -> None:
        with mock.patch.dict(
            os.environ,
            {
                "SHIPYARD_SIGNING_KEYCHAIN": "/tmp/local.keychain-db",
                "SHIPYARD_SIGNING_KEYCHAIN_READY": "1",
            },
            clear=True,
        ):
            with self.assertRaises(SystemExit):
                with package_release.prepared_signing_keychain():
                    pass

    def test_disposable_keychain_is_preserved_if_search_list_still_references_it(self) -> None:
        with tempfile.TemporaryDirectory() as parent:
            temp = Path(parent) / "lease"
            temp.mkdir()
            keychain = temp / "shipyard-signing.keychain-db"
            keychain.touch()
            with mock.patch.dict(
                os.environ,
                {
                    "SHIPYARD_SIGNING_P12": "/tmp/signing.p12",
                    "SHIPYARD_SIGNING_P12_PASSWORD": "p12-secret",
                },
                clear=True,
            ), mock.patch.object(
                package_release.tempfile,
                "mkdtemp",
                return_value=str(temp),
            ), mock.patch.object(
                package_release,
                "create_disposable_signing_keychain",
                return_value=(keychain, "keychain-secret"),
            ), mock.patch.object(
                package_release,
                "signing_keychain_first",
                return_value=package_release.contextlib.nullcontext(),
            ), mock.patch.object(
                package_release,
                "signing_keychain_is_listed",
                return_value=True,
            ), mock.patch.object(
                package_release,
                "delete_notary_keychain",
            ) as delete, mock.patch.object(package_release.shutil, "rmtree") as rmtree:
                with package_release.prepared_signing_keychain():
                    pass

            delete.assert_not_called()
            rmtree.assert_not_called()
            self.assertTrue(keychain.exists())

    def test_disposable_keychain_is_deleted_only_after_restored_search_list(self) -> None:
        with tempfile.TemporaryDirectory() as parent:
            temp = Path(parent) / "lease"
            temp.mkdir()
            keychain = temp / "shipyard-signing.keychain-db"
            keychain.touch()
            with mock.patch.dict(
                os.environ,
                {
                    "SHIPYARD_SIGNING_P12": "/tmp/signing.p12",
                    "SHIPYARD_SIGNING_P12_PASSWORD": "p12-secret",
                },
                clear=True,
            ), mock.patch.object(
                package_release.tempfile,
                "mkdtemp",
                return_value=str(temp),
            ), mock.patch.object(
                package_release,
                "create_disposable_signing_keychain",
                return_value=(keychain, "keychain-secret"),
            ), mock.patch.object(
                package_release,
                "signing_keychain_first",
                return_value=package_release.contextlib.nullcontext(),
            ), mock.patch.object(
                package_release,
                "signing_keychain_is_listed",
                return_value=False,
            ), mock.patch.object(
                package_release,
                "delete_notary_keychain",
            ) as delete, mock.patch.object(package_release.shutil, "rmtree") as rmtree:
                with package_release.prepared_signing_keychain():
                    pass

            delete.assert_called_once_with(keychain)
            rmtree.assert_called_once_with(temp, ignore_errors=True)


if __name__ == "__main__":
    unittest.main()

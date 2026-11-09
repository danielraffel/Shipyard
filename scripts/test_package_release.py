#!/usr/bin/env python3
from __future__ import annotations

import os
import shutil
import stat
import subprocess
import tempfile
import time
import unittest
from contextlib import redirect_stdout
from io import StringIO
from pathlib import Path
from shlex import quote as shlex_quote
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

    def test_packaging_rejects_semantic_version_mismatch_before_copy(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            binary = root / "shipyard"
            binary.write_text("#!/bin/sh\necho 'shipyard 0.127.0'\n", encoding="utf-8")
            binary.chmod(0o755)
            companion = root / "shipyard-workstream-provider"
            companion.write_text(
                "#!/bin/sh\necho 'shipyard-workstream-provider 0.127.1'\n",
                encoding="utf-8",
            )
            companion.chmod(0o755)
            dist = root / "dist"
            args = package_release.parse_args(
                [
                    "--skip-build",
                    "--binary",
                    str(binary),
                    "--companion-binary",
                    str(companion),
                    "--target",
                    "linux-x64",
                    "--dist-dir",
                    str(dist),
                ]
            )

            with self.assertRaisesRegex(SystemExit, "version mismatch"):
                package_release.package(args)

            self.assertFalse(dist.exists())

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

    def test_detach_retries_a_busy_volume_until_it_unmounts(self) -> None:
        calls: list[list[str]] = []
        mounted = iter([True, True, False])
        sleeps: list[float] = []

        def fake_run(args, **_kwargs):
            calls.append(args)
            return subprocess.CompletedProcess(args, 16, "", "hdiutil: couldn't unmount: Resource busy")

        with mock.patch.object(package_release.subprocess, "run", side_effect=fake_run):
            package_release.detach_dmg(
                Path("/tmp/mnt"),
                sleep=sleeps.append,
                is_mounted=lambda _path: next(mounted),
            )

        self.assertEqual(len(calls), 3)
        self.assertEqual(sleeps, [1.0, 2.0])
        self.assertNotIn("-force", calls[-1])

    def test_detach_forces_the_last_attempt_and_names_the_refusal(self) -> None:
        calls: list[list[str]] = []

        def fake_run(args, **_kwargs):
            calls.append(args)
            return subprocess.CompletedProcess(args, 16, "", "hdiutil: couldn't unmount: Resource busy")

        with mock.patch.object(package_release.subprocess, "run", side_effect=fake_run):
            with self.assertRaises(package_release.CommandFailed) as raised:
                package_release.detach_dmg(
                    Path("/tmp/mnt"),
                    sleep=lambda _seconds: None,
                    is_mounted=lambda _path: True,
                )

        self.assertEqual(len(calls), package_release.DETACH_ATTEMPTS)
        self.assertEqual(calls[-1][-1], "-force")
        self.assertIn("could not detach /tmp/mnt", str(raised.exception))
        self.assertIn("Resource busy", str(raised.exception))

    def _smoke_with_detach(self, detach_error, still_mounted: bool) -> tuple[object, list[Path]]:
        created: list[Path] = []
        real_mkdtemp = package_release.tempfile.mkdtemp

        def fake_mkdtemp(**kwargs):
            path = Path(real_mkdtemp(**kwargs))
            created.append(path)
            return str(path)

        def fake_detach(_mount):
            if detach_error is not None:
                raise detach_error

        outcome: object
        with mock.patch.object(package_release, "require_commands"), \
                mock.patch.object(package_release, "run", return_value=""), \
                mock.patch.object(package_release.tempfile, "mkdtemp", side_effect=fake_mkdtemp), \
                mock.patch.object(package_release, "smoke_binary_pair", return_value="smoke ok"), \
                mock.patch.object(package_release, "detach_dmg", side_effect=fake_detach), \
                mock.patch.object(package_release.os.path, "ismount", return_value=still_mounted):
            try:
                outcome = package_release.smoke_dmg(
                    Path("fake.dmg"),
                    ("shipyard", "shipyard-workstream-provider"),
                    ci_mode=False,
                )
            except BaseException as error:  # noqa: BLE001 - the test inspects it
                outcome = error
        return outcome, created

    def test_dmg_smoke_never_walks_into_a_volume_it_could_not_detach(self) -> None:
        refused = package_release.CommandFailed("could not detach /x/mnt after 5 attempts: busy")
        outcome, created = self._smoke_with_detach(refused, still_mounted=True)
        try:
            self.assertIs(outcome, refused, "the detach failure is the error, not EROFS")
            self.assertTrue(created[0].exists(), "a still-mounted temp dir is left in place")
        finally:
            shutil.rmtree(created[0], ignore_errors=True)

    def test_dmg_smoke_removes_its_temp_dir_once_detached(self) -> None:
        outcome, created = self._smoke_with_detach(None, still_mounted=False)
        self.assertEqual(outcome, "smoke ok")
        self.assertFalse(created[0].exists())

    ACCEPTED = (
        "Submission ID received\n"
        "  id: 2efe2717-52ef-43a5-96dc-0797e4ca1041\n"
        "Processing complete\n"
        "  id: 2efe2717-52ef-43a5-96dc-0797e4ca1041\n"
        "  status: Accepted\n"
    )
    SUBMISSION_ID = "2efe2717-52ef-43a5-96dc-0797e4ca1041"

    def test_notarize_uses_keychain_profile_for_long_running_submit(self) -> None:
        calls: list[list[str]] = []
        streamed: list[list[str]] = []

        def fake_run(args: list[str], **_kwargs: object) -> str:
            calls.append(args)
            return ""

        def fake_streamed(args: list[str], **kwargs: object) -> tuple[int, str]:
            streamed.append(args)
            if args[:3] == ["xcrun", "notarytool", "store-credentials"]:
                self.assertEqual(
                    kwargs["timeout"], package_release.NOTARY_CREDENTIAL_TIMEOUT_SECS
                )
                return 0, ""
            return 0, self.ACCEPTED

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
            package_release, "run", side_effect=fake_run
        ), mock.patch.object(
            package_release, "run_streamed", side_effect=fake_streamed
        ), redirect_stdout(StringIO()):
            package_release.notarize_and_staple(Path("shipyard.dmg"))

        store = streamed[0]
        self.assertEqual(store[:3], ["xcrun", "notarytool", "store-credentials"])
        self.assertEqual(len(streamed), 2)
        submit = streamed[1]
        self.assertEqual(submit[:3], ["xcrun", "notarytool", "submit"])
        self.assertIn("--password", store)
        self.assertNotIn("--password", submit)
        self.assertIn("--keychain-profile", submit)
        self.assertIn("--keychain", submit)
        self.assertIn("--wait", submit)
        self.assertEqual(submit[submit.index("--timeout") + 1], "20m")
        self.assertEqual(package_release.NOTARY_WAIT_TIMEOUT, "20m")
        self.assertIn(["xcrun", "stapler", "staple", "shipyard.dmg"], calls)
        delete_keychain.assert_called_once_with(Path("/tmp/notary.keychain-db"))

    def test_notarize_api_key_mode_uses_direct_credentials(self) -> None:
        calls: list[list[str]] = []
        streamed: list[list[str]] = []

        def fake_run(args: list[str], **_kwargs: object) -> str:
            calls.append(args)
            return ""

        def fake_streamed(args: list[str], **_kwargs: object) -> tuple[int, str]:
            streamed.append(args)
            return 0, self.ACCEPTED

        with mock.patch.dict(
            os.environ,
            {
                "SHIPYARD_NOTARIZE_KEY_PATH": "/tmp/AuthKey_TEST.p8",
                "SHIPYARD_NOTARIZE_KEY_ID": "KEY123",
                "SHIPYARD_NOTARIZE_ISSUER_ID": "issuer-uuid",
            },
            clear=True,
        ), mock.patch.object(package_release, "run", side_effect=fake_run), \
                mock.patch.object(package_release, "run_streamed", side_effect=fake_streamed), \
                redirect_stdout(StringIO()):
            package_release.notarize_and_staple(Path("shipyard.dmg"))

        submit = streamed[0]
        self.assertEqual(submit[:3], ["xcrun", "notarytool", "submit"])
        self.assertIn("--key", submit)
        self.assertIn("--key-id", submit)
        self.assertIn("--issuer", submit)
        self.assertNotIn("--password", submit)
        self.assertEqual(submit[submit.index("--timeout") + 1], "20m")
        self.assertNotIn("store-credentials", " ".join(" ".join(call) for call in calls))

    def _submit(self, results, *, attempts=3):
        """Drive submit_for_notarization over scripted (returncode, output) pairs."""
        streamed: list[list[str]] = []
        sleeps: list[float] = []
        queue = list(results)

        def fake_streamed(args: list[str], **kwargs: object) -> tuple[int | None, str]:
            streamed.append(args)
            if args[:3] == ["xcrun", "notarytool", "log"]:
                return 0, '{"issues": [{"message": "The binary is not signed."}]}'
            self.assertEqual(kwargs["timeout"], package_release.NOTARY_ATTEMPT_TIMEOUT_SECS)
            return queue.pop(0)

        out = StringIO()
        with mock.patch.object(package_release, "run_streamed", side_effect=fake_streamed), \
                mock.patch.dict(os.environ, {}, clear=True), redirect_stdout(out):
            try:
                result = package_release.submit_for_notarization(
                    Path("shipyard.dmg"),
                    ["--key", "k.p8"],
                    attempts=attempts,
                    sleep=sleeps.append,
                )
                error = None
            except SystemExit as exc:
                result, error = None, exc
        return result, error, streamed, sleeps, out.getvalue()

    def test_notarization_accepted_logs_the_submission_id(self) -> None:
        result, error, streamed, sleeps, out = self._submit([(0, self.ACCEPTED)])
        self.assertIsNone(error)
        self.assertEqual(result, self.SUBMISSION_ID)
        self.assertEqual(len(streamed), 1)
        self.assertEqual(sleeps, [])
        self.assertIn(f"notarization submission id: {self.SUBMISSION_ID}", out)
        self.assertIn(f"xcrun notarytool log {self.SUBMISSION_ID}", out)

    def test_hung_upload_with_no_id_is_resubmitted_with_backoff(self) -> None:
        result, error, streamed, sleeps, _ = self._submit(
            [(None, "Conducting pre-submission checks...\n"), (0, self.ACCEPTED)]
        )
        self.assertIsNone(error)
        self.assertEqual(result, self.SUBMISSION_ID)
        self.assertEqual([args[2] for args in streamed], ["submit", "submit"])
        self.assertEqual(sleeps, [60.0])

    def test_id_without_a_finished_upload_is_uploaded_again(self) -> None:
        # notarytool prints the id before the upload; a stall mid-upload
        # leaves Apple without the file, so waiting on that id cannot succeed.
        mid_upload = (
            "Submission ID received\n"
            f"  id: {self.SUBMISSION_ID}\n"
            "Upload progress: 42.00% (1.2 MB of 2.9 MB)\n"
        )
        result, error, streamed, sleeps, out = self._submit(
            [(None, mid_upload), (0, self.ACCEPTED)]
        )
        self.assertIsNone(error)
        self.assertEqual([args[2] for args in streamed], ["submit", "submit"])
        self.assertIn(f"notarization submission id: {self.SUBMISSION_ID}", out)
        self.assertIn("did not complete", out)

    def test_auth_failure_is_not_retried(self) -> None:
        result, error, streamed, sleeps, _ = self._submit(
            [(69, "Error: HTTP status code: 401. Invalid credentials.\n")]
        )
        self.assertIsNotNone(error)
        self.assertEqual(len(streamed), 1)
        self.assertEqual(sleeps, [])
        self.assertIn("non-retryable", str(error))

    def test_store_credentials_is_bounded_and_retried_once(self) -> None:
        calls: list[tuple[list[str], float]] = []
        sleeps: list[float] = []
        results = [(None, ""), (0, "")]

        def fake_streamed(args: list[str], **kwargs: object):
            calls.append((args, kwargs["timeout"]))
            return results.pop(0)

        with mock.patch.dict(
            os.environ,
            {
                "SHIPYARD_NOTARIZE_APPLE_ID": "apple@example.com",
                "SHIPYARD_NOTARIZE_TEAM_ID": "TEAM123",
                "SHIPYARD_NOTARIZE_APP_PASSWORD": "app-secret",
            },
            clear=True,
        ), mock.patch.object(package_release, "run_streamed", side_effect=fake_streamed), \
                redirect_stdout(StringIO()):
            package_release.store_notary_credentials(
                "profile", Path("/tmp/k.keychain-db"), sleep=sleeps.append
            )
        self.assertEqual(len(calls), 2)
        self.assertTrue(all(t == package_release.NOTARY_CREDENTIAL_TIMEOUT_SECS for _, t in calls))
        self.assertEqual(sleeps, [package_release.NOTARY_CREDENTIAL_BACKOFF_SECS])

    def test_retry_after_a_finished_upload_waits_instead_of_resubmitting(self) -> None:
        in_progress = (
            "Submission ID received\n"
            f"  id: {self.SUBMISSION_ID}\n"
            "Successfully uploaded file\n"
            f"  id: {self.SUBMISSION_ID}\n"
            "Waiting for processing to complete.\n"
            "Current status: In Progress...\n"
        )
        result, error, streamed, sleeps, _ = self._submit(
            [(None, in_progress), (0, self.ACCEPTED)]
        )
        self.assertIsNone(error)
        self.assertEqual(streamed[0][2], "submit")
        self.assertEqual(streamed[1][:4], ["xcrun", "notarytool", "wait", self.SUBMISSION_ID])
        self.assertNotIn("--wait", streamed[1])
        self.assertEqual(streamed[1][streamed[1].index("--timeout") + 1], "20m")
        self.assertEqual(sleeps, [60.0])

    def test_transient_upload_error_is_retried_at_most_twice(self) -> None:
        failure = (1, "Error: HTTP status code: 503. Service Unavailable\n")
        result, error, streamed, sleeps, _ = self._submit([failure, failure, failure])
        self.assertIsNotNone(error)
        self.assertEqual(len(streamed), 3)
        self.assertEqual(sleeps, [60.0, 180.0])
        message = str(error)
        self.assertIn("did not complete after 3 attempts", message)
        self.assertIn("submission id never received", message)
        self.assertIn("attempt 3: notarytool exited 1", message)

    def test_invalid_verdict_fails_loudly_with_the_notary_log_and_no_retry(self) -> None:
        invalid = (
            f"  id: {self.SUBMISSION_ID}\n"
            "Processing complete\n"
            f"  id: {self.SUBMISSION_ID}\n"
            "  status: Invalid\n"
        )
        result, error, streamed, sleeps, _ = self._submit([(1, invalid)])
        self.assertIsNotNone(error)
        self.assertEqual(sleeps, [])
        self.assertEqual(
            streamed[-1][:4], ["xcrun", "notarytool", "log", self.SUBMISSION_ID]
        )
        self.assertIn("--key", streamed[-1])
        message = str(error)
        self.assertIn("Notarization Invalid", message)
        self.assertIn(self.SUBMISSION_ID, message)
        self.assertIn("The binary is not signed.", message)

    def test_current_status_line_is_not_read_as_a_verdict(self) -> None:
        self.assertIsNone(
            package_release.notary_final_status("Current status: Accepted...\n")
        )
        self.assertEqual(
            package_release.notary_final_status("Current status: In Progress\n  status: Accepted\n"),
            "Accepted",
        )

    def test_submission_id_is_appended_to_the_step_summary(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            summary = Path(temp) / "summary.md"
            with mock.patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(summary)}), \
                    redirect_stdout(StringIO()):
                package_release.report_notary_submission(self.SUBMISSION_ID)
            self.assertIn(self.SUBMISSION_ID, summary.read_text(encoding="utf-8"))

    def test_run_streamed_kills_a_stalled_process_group_and_keeps_its_output(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            marker = Path(temp) / "child.pid"
            # The grandchild stands in for an upload helper that outlives a
            # killed parent: the whole process group must go.
            script = (
                f"sleep 60 & echo $! > {shlex_quote(str(marker))}; "
                "echo 'Submission ID received'; sleep 60"
            )
            out = StringIO()
            started = time.monotonic()
            with redirect_stdout(out):
                returncode, output = package_release.run_streamed(
                    ["/bin/sh", "-c", script], timeout=1.0
                )
            elapsed = time.monotonic() - started
            self.assertIsNone(returncode)
            self.assertLess(elapsed, 20)
            self.assertIn("Submission ID received", output)
            self.assertIn("Submission ID received", out.getvalue())
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
                self.fail("grandchild outlived the timeout kill")

    def test_run_streamed_closes_stdin_and_redacts(self) -> None:
        # Hold the parent's stdin on a pipe nobody writes, as a runner's can
        # be: a child that inherited it would block in `read` until the cap.
        read_fd, write_fd = os.pipe()
        saved_stdin = os.dup(0)
        os.dup2(read_fd, 0)
        try:
            with mock.patch.dict(os.environ, {"SHIPYARD_NOTARIZE_APP_PASSWORD": "app-secret"}), \
                    redirect_stdout(StringIO()) as out:
                returncode, output = package_release.run_streamed(
                    ["/bin/sh", "-c", "read line || echo eof; echo app-secret"], timeout=5
                )
        finally:
            os.dup2(saved_stdin, 0)
            for fd in (saved_stdin, read_fd, write_fd):
                os.close(fd)
        self.assertEqual(returncode, 0)
        self.assertIn("eof", output)
        self.assertNotIn("app-secret", output)
        self.assertNotIn("app-secret", out.getvalue())

    def test_run_timeout_raises_command_failed(self) -> None:
        with self.assertRaisesRegex(package_release.CommandFailed, "timed out after"):
            package_release.run(["/bin/sleep", "30"], timeout=0.5)

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
                {
                    "SHIPYARD_SIGNING_KEYCHAIN": str(keychain),
                    "SHIPYARD_SIGNING_HOME": "/tmp/isolated signing home",
                },
                clear=True,
            ), mock.patch.object(package_release, "run", side_effect=fake_run) as run:
                with package_release.signing_keychain_first():
                    calls.append(["inside"])

        prefix = ["security", "list-keychains", "-d", "user", "-s"]
        set_calls = [call for call in calls if call[:5] == prefix]
        self.assertEqual(str(keychain.resolve()), set_calls[0][5])
        self.assertEqual(set_calls[-1], [*prefix, "/tmp/login.keychain-db"])
        for call in run.call_args_list:
            if call.args[0][:2] == ["security", "list-keychains"]:
                self.assertEqual(call.kwargs["env"]["HOME"], "/tmp/isolated signing home")

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
                {
                    "SHIPYARD_SIGNING_KEYCHAIN": str(keychain),
                    "SHIPYARD_SIGNING_HOME": "/tmp/isolated signing home",
                },
                clear=True,
            ), mock.patch.object(package_release, "run", side_effect=fake_run):
                with self.assertRaises(SystemExit):
                    with package_release.signing_keychain_first():
                        pass

        self.assertFalse(any("-s" in call for call in calls))

    def test_signing_keychain_is_listed_uses_isolated_home(self) -> None:
        with mock.patch.dict(
            os.environ,
            {
                "SHIPYARD_SIGNING_HOME": "/tmp/isolated signing home",
            },
            clear=True,
        ), mock.patch.object(
            package_release,
            "run",
            return_value='"/tmp/ephemeral.keychain-db"\n',
        ) as run:
            self.assertTrue(
                package_release.signing_keychain_is_listed(
                    Path("/tmp/ephemeral.keychain-db")
                )
            )
        self.assertEqual(run.call_args.kwargs["env"]["HOME"], "/tmp/isolated signing home")

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

    def test_ci_prepared_keychain_wins_over_a_p12_in_the_environment(self) -> None:
        with mock.patch.dict(
            os.environ,
            {
                "SHIPYARD_SIGNING_KEYCHAIN": "/tmp/ci.keychain-db",
                "SHIPYARD_SIGNING_KEYCHAIN_READY": "1",
                "CI": "true",
                "SHIPYARD_SIGNING_P12": "/tmp/host-local.p12",
                "SHIPYARD_SIGNING_P12_PASSWORD": "host-secret",
            },
            clear=True,
        ), mock.patch.object(package_release, "signing_keychain_first") as prepared, \
                mock.patch.object(package_release, "create_disposable_signing_keychain") as disposable:
            with package_release.prepared_signing_keychain():
                self.assertEqual(os.environ["SHIPYARD_SIGNING_KEYCHAIN"], "/tmp/ci.keychain-db")
        prepared.assert_called_once_with()
        disposable.assert_not_called()

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

#!/usr/bin/env python3
from __future__ import annotations

import importlib.machinery
import importlib.util
import io
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from contextlib import contextmanager, redirect_stderr, redirect_stdout
from io import StringIO
from pathlib import Path
from unittest import mock


SCRIPT_PATH = Path(__file__).with_name("shipyard-github-app-token")
LOADER = importlib.machinery.SourceFileLoader("shipyard_github_app_token", str(SCRIPT_PATH))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
assert SPEC is not None
shipyard_github_app_token = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(shipyard_github_app_token)


@contextmanager
def exclusive_writer_domain_audit(state_dir: Path):
    with tempfile.TemporaryDirectory() as control_temp:
        control = Path(control_temp)
        ready = control / "ready"
        release = control / "release"
        holder = subprocess.Popen(
            [
                sys.executable,
                "-c",
                """
import fcntl
import os
import pathlib
import sys
import time

state_dir = pathlib.Path(sys.argv[1])
ready = pathlib.Path(sys.argv[2])
release = pathlib.Path(sys.argv[3])
state_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
turnstile = os.open(state_dir / '.sandbox-writer-domain.turnstile.lock', os.O_CREAT | os.O_RDWR, 0o600)
domain = os.open(state_dir / '.sandbox-writer-domain.lock', os.O_CREAT | os.O_RDWR, 0o600)
fcntl.flock(turnstile, fcntl.LOCK_EX)
fcntl.flock(domain, fcntl.LOCK_EX)
ready.write_text('ready', encoding='utf-8')
while not release.exists():
    time.sleep(0.01)
fcntl.flock(domain, fcntl.LOCK_UN)
fcntl.flock(turnstile, fcntl.LOCK_UN)
os.close(domain)
os.close(turnstile)
""",
                str(state_dir),
                str(ready),
                str(release),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        deadline = time.monotonic() + 5
        while not ready.exists():
            if holder.poll() is not None:
                stdout, stderr = holder.communicate()
                raise AssertionError(
                    f"exclusive audit exited before ready: {stdout=} {stderr=}"
                )
            if time.monotonic() >= deadline:
                holder.terminate()
                holder.wait(timeout=5)
                raise AssertionError("timed out waiting for exclusive audit")
            time.sleep(0.01)
        try:
            yield release
        finally:
            release.touch()
            stdout, stderr = holder.communicate(timeout=5)
            if holder.returncode != 0:
                raise AssertionError(
                    f"exclusive audit failed: {stdout=} {stderr=}"
                )


class GitHubAppTokenHelperTests(unittest.TestCase):
    def cache_payload(self) -> dict[str, str]:
        expires = (
            shipyard_github_app_token.datetime.datetime.now(
                shipyard_github_app_token.datetime.timezone.utc
            )
            + shipyard_github_app_token.datetime.timedelta(minutes=30)
        ).isoformat().replace("+00:00", "Z")
        return {
            "token": "ghs_personal",
            "kind": "github-app-installation",
            "expires_at": expires,
            "api_url": "https://api.github.com",
            "app_id": "3878000",
            "installation_id": "135929628",
            "repository": "danielraffel/Shipyard",
        }

    def test_public_payload_includes_non_secret_installation_identity(self) -> None:
        public = shipyard_github_app_token.public_token_payload(
            {
                "token": "ghs_test",
                "kind": "github-app-installation",
                "installation_id": "135929628",
            }
        )
        self.assertEqual(public["installation_id"], "135929628")

    def test_base64url_bytes_strips_padding(self) -> None:
        self.assertEqual(shipyard_github_app_token.base64url_bytes(b"ship"), "c2hpcA")

    def test_required_value_reads_environment_fallback(self) -> None:
        with mock.patch.dict(os.environ, {"SHIPYARD_GITHUB_APP_ID": "12345"}, clear=True):
            value = shipyard_github_app_token.required_value(
                None,
                "SHIPYARD_GITHUB_APP_ID",
                "--app-id",
            )

        self.assertEqual(value, "12345")

    def test_windows_disk_cache_fails_closed_but_memory_only_is_allowed(self) -> None:
        shipyard_github_app_token.validate_cache_platform(None, "win32")
        with self.assertRaises(shipyard_github_app_token.HelperError) as ctx:
            shipyard_github_app_token.validate_cache_platform(
                Path("C:/private/cache"), "win32"
            )
        self.assertIn("private ACL", str(ctx.exception))
        shipyard_github_app_token.validate_cache_platform(
            Path("/private/cache"), "darwin"
        )

    @unittest.skipIf(os.name == "nt", "POSIX permission invariant")
    def test_private_key_requires_closed_file_and_directory_modes(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp) / "github-apps"
            directory.mkdir(mode=0o700)
            key = directory / "app.pem"
            key.write_text("fake-key", encoding="utf-8")
            key.chmod(0o600)
            shipyard_github_app_token.validate_private_key(key)

            key.chmod(0o644)
            with self.assertRaises(shipyard_github_app_token.HelperError) as ctx:
                shipyard_github_app_token.validate_private_key(key)
            self.assertIn("0600", str(ctx.exception))

            key.chmod(0o600)
            directory.chmod(0o755)
            with self.assertRaises(shipyard_github_app_token.HelperError) as ctx:
                shipyard_github_app_token.validate_private_key(key)
            self.assertIn("0700", str(ctx.exception))

    def test_lookup_repo_installation_requires_repo_slug(self) -> None:
        with self.assertRaises(shipyard_github_app_token.HelperError) as ctx:
            shipyard_github_app_token.lookup_repo_installation(
                "https://api.github.com",
                "jwt",
                "not-a-slug",
            )

        self.assertIn("owner/name", str(ctx.exception))

    def test_repo_lookup_outranks_stale_environment_installation(self) -> None:
        with mock.patch.object(
            shipyard_github_app_token,
            "lookup_repo_installation",
            return_value="135929628",
        ) as lookup:
            installation_id = shipyard_github_app_token.resolve_installation_id(
                "https://api.github.com",
                "jwt",
                "danielraffel/Shipyard",
                None,
                "147677577",
            )

        self.assertEqual(installation_id, "135929628")
        lookup.assert_called_once_with(
            "https://api.github.com",
            "jwt",
            "danielraffel/Shipyard",
        )

    def test_explicit_installation_pin_must_match_repo_lookup(self) -> None:
        with mock.patch.object(
            shipyard_github_app_token,
            "lookup_repo_installation",
            return_value="135929628",
        ):
            with self.assertRaises(shipyard_github_app_token.HelperError) as ctx:
                shipyard_github_app_token.resolve_installation_id(
                    "https://api.github.com",
                    "jwt",
                    "danielraffel/Shipyard",
                    "147677577",
                    None,
                )

        self.assertIn("does not match", str(ctx.exception))
        self.assertNotIn("ghs_", str(ctx.exception))

    def test_repo_slug_rejects_extra_path_components(self) -> None:
        with self.assertRaises(shipyard_github_app_token.HelperError):
            shipyard_github_app_token.validate_repo_slug(
                "danielraffel/Shipyard/hooks"
            )

    def test_empty_repo_does_not_fall_back_to_pinned_installation(self) -> None:
        with self.assertRaises(shipyard_github_app_token.HelperError):
            shipyard_github_app_token.resolve_installation_id(
                "https://api.github.com",
                "jwt",
                "",
                None,
                "147677577",
            )

    def test_repo_less_installation_id_must_be_numeric(self) -> None:
        with self.assertRaises(shipyard_github_app_token.HelperError) as ctx:
            shipyard_github_app_token.resolve_installation_id(
                "https://api.github.com",
                "jwt",
                None,
                "../installations/1",
                None,
            )

        self.assertIn("positive integer", str(ctx.exception))

    @unittest.skipIf(os.name == "nt", "POSIX permission invariant")
    def test_disk_cache_is_partitioned_by_repository_and_mode_0600(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            cache_dir = Path(temp) / "cache"
            expires = (
                shipyard_github_app_token.datetime.datetime.now(
                    shipyard_github_app_token.datetime.timezone.utc
                )
                + shipyard_github_app_token.datetime.timedelta(minutes=30)
            ).isoformat().replace("+00:00", "Z")
            for repo, installation_id, token in (
                ("danielraffel/Shipyard", "135929628", "ghs_personal"),
                ("Generous-Corp/pulp", "147677577", "ghs_org"),
            ):
                payload = {
                    "token": token,
                    "kind": "github-app-installation",
                    "expires_at": expires,
                    "api_url": "https://api.github.com",
                    "app_id": "3878000",
                    "installation_id": installation_id,
                    "repository": repo,
                }
                shipyard_github_app_token.store_cached_token(
                    cache_dir,
                    "https://api.github.com",
                    "3878000",
                    repo,
                    installation_id,
                    payload,
                )

            personal = shipyard_github_app_token.load_cached_token(
                cache_dir,
                "https://api.github.com",
                "3878000",
                "danielraffel/Shipyard",
                None,
                "147677577",
            )
            organization = shipyard_github_app_token.load_cached_token(
                cache_dir,
                "https://api.github.com",
                "3878000",
                "Generous-Corp/pulp",
                None,
                None,
            )

            self.assertEqual(personal["token"], "ghs_personal")
            self.assertEqual(organization["token"], "ghs_org")
            entries = list(cache_dir.glob("installation-token-*.json"))
            self.assertEqual(len(entries), 2)
            self.assertEqual(cache_dir.stat().st_mode & 0o777, 0o700)
            self.assertTrue(all(entry.stat().st_mode & 0o777 == 0o600 for entry in entries))

    @unittest.skipIf(os.name == "nt", "POSIX symlink invariant")
    def test_disk_cache_refuses_symlink_entry(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            cache_dir = Path(temp) / "cache"
            cache_dir.mkdir(mode=0o700)
            target = Path(temp) / "other-token.json"
            target.write_text("{}", encoding="utf-8")
            target.chmod(0o600)
            entry = shipyard_github_app_token.cache_file(
                cache_dir,
                "https://api.github.com",
                "3878000",
                "repo:danielraffel/Shipyard",
            )
            entry.symlink_to(target)

            with self.assertRaises(shipyard_github_app_token.HelperError) as ctx:
                shipyard_github_app_token.load_cached_token(
                    cache_dir,
                    "https://api.github.com",
                    "3878000",
                    "danielraffel/Shipyard",
                    None,
                    None,
                )

        self.assertIn("not a symlink", str(ctx.exception))

    def test_disk_cache_refuses_untrusted_kind_provenance(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            cache_dir = Path(temp) / "cache"
            expires = (
                shipyard_github_app_token.datetime.datetime.now(
                    shipyard_github_app_token.datetime.timezone.utc
                )
                + shipyard_github_app_token.datetime.timedelta(minutes=30)
            ).isoformat().replace("+00:00", "Z")
            payload = {
                "token": "ghs_personal",
                "kind": "ambient-user",
                "expires_at": expires,
                "api_url": "https://api.github.com",
                "app_id": "3878000",
                "installation_id": "135929628",
                "repository": "danielraffel/Shipyard",
            }
            shipyard_github_app_token.store_cached_token(
                cache_dir,
                "https://api.github.com",
                "3878000",
                "danielraffel/Shipyard",
                "135929628",
                payload,
            )

            with self.assertRaises(shipyard_github_app_token.HelperError) as ctx:
                shipyard_github_app_token.load_cached_token(
                    cache_dir,
                    "https://api.github.com",
                    "3878000",
                    "danielraffel/Shipyard",
                    None,
                    None,
                )

        self.assertIn("kind provenance", str(ctx.exception))

    def test_cache_directory_concurrent_first_creation_is_safe(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            cache_dir = Path(temp) / "cache"
            original_mkdir = Path.mkdir

            def concurrent_create(path: Path, *args: object, **kwargs: object) -> None:
                original_mkdir(path, *args, **kwargs)
                raise FileExistsError(path)

            with mock.patch.object(Path, "mkdir", concurrent_create):
                shipyard_github_app_token.ensure_private_cache_dir(cache_dir)

            self.assertTrue(cache_dir.is_dir())
            if os.name != "nt":
                self.assertEqual(cache_dir.stat().st_mode & 0o777, 0o700)

    def test_cache_miss_is_read_only(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            cache_dir = Path(temp) / "missing-cache"
            cached = shipyard_github_app_token.load_cached_token(
                cache_dir,
                "https://api.github.com",
                "3878000",
                "danielraffel/Shipyard",
                None,
                None,
            )

            self.assertIsNone(cached)
            self.assertFalse(cache_dir.exists())

    @unittest.skipIf(os.name == "nt", "POSIX writer-domain invariant")
    def test_protected_cache_write_refuses_after_bounded_audit_wait(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            home = Path(temp) / "home"
            home.mkdir()
            cache_dir = home / ".config" / "shipyard" / "token-cache"
            state_dir = (
                home / "Library" / "Application Support" / "shipyard"
                if sys.platform == "darwin"
                else home / ".local" / "state" / "shipyard"
            )
            with mock.patch.dict(os.environ, {"HOME": str(home)}, clear=False):
                with exclusive_writer_domain_audit(state_dir), mock.patch.object(
                    shipyard_github_app_token,
                    "WRITER_DOMAIN_ACQUIRE_TIMEOUT_SECONDS",
                    0.05,
                ):
                    started = time.monotonic()
                    with self.assertRaises(shipyard_github_app_token.HelperError) as ctx:
                        shipyard_github_app_token.store_cached_token(
                            cache_dir,
                            "https://api.github.com",
                            "3878000",
                            "danielraffel/Shipyard",
                            "135929628",
                            self.cache_payload(),
                        )
                    elapsed = time.monotonic() - started

            self.assertGreaterEqual(elapsed, 0.04)
            self.assertTrue(
                str(ctx.exception).startswith("sandbox_writer_domain_overlap:")
            )
            self.assertFalse(cache_dir.exists())

    @unittest.skipIf(os.name == "nt", "POSIX writer-domain invariant")
    def test_protected_cache_write_waits_then_succeeds_after_audit_release(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            home = Path(temp) / "home"
            home.mkdir()
            cache_dir = home / ".config" / "shipyard" / "token-cache"
            state_dir = (
                home / "Library" / "Application Support" / "shipyard"
                if sys.platform == "darwin"
                else home / ".local" / "state" / "shipyard"
            )
            with mock.patch.dict(os.environ, {"HOME": str(home)}, clear=False):
                with exclusive_writer_domain_audit(state_dir) as release:
                    releaser = threading.Timer(
                        0.1,
                        release.touch,
                    )
                    releaser.start()
                    started = time.monotonic()
                    try:
                        shipyard_github_app_token.store_cached_token(
                            cache_dir,
                            "https://api.github.com",
                            "3878000",
                            "danielraffel/Shipyard",
                            "135929628",
                            self.cache_payload(),
                        )
                    finally:
                        releaser.join(timeout=2)
                    elapsed = time.monotonic() - started

            destination = shipyard_github_app_token.cache_file(
                cache_dir,
                "https://api.github.com",
                "3878000",
                "repo:danielraffel/Shipyard",
            )
            self.assertGreaterEqual(elapsed, 0.08)
            self.assertEqual(json.loads(destination.read_text())["token"], "ghs_personal")

    def test_platform_ca_fallback_augments_default_context(self) -> None:
        context = mock.Mock()
        with tempfile.TemporaryDirectory() as temp:
            ca_file = Path(temp) / "cert.pem"
            ca_file.write_text("test-ca", encoding="utf-8")
            with mock.patch.object(
                shipyard_github_app_token,
                "SYSTEM_CA_FILES",
                (ca_file,),
            ):
                resolved = shipyard_github_app_token.add_platform_ca_files(context)

        self.assertIs(resolved, context)
        context.load_verify_locations.assert_called_once_with(cafile=str(ca_file))

    def test_missing_default_store_requires_no_explicit_or_loaded_trust(self) -> None:
        context = mock.Mock()
        context.cert_store_stats.return_value = {"x509_ca": 0}
        paths = mock.Mock(cafile="/missing/cert.pem", capath="/missing/certs")

        with mock.patch.dict(os.environ, {}, clear=True), mock.patch.object(
            shipyard_github_app_token.ssl,
            "get_default_verify_paths",
            return_value=paths,
        ):
            self.assertTrue(
                shipyard_github_app_token.default_trust_store_is_missing(context)
            )

        with mock.patch.dict(
            os.environ,
            {"SSL_CERT_FILE": "/operator/pinned.pem"},
            clear=True,
        ):
            self.assertFalse(
                shipyard_github_app_token.default_trust_store_is_missing(context)
            )

        context.cert_store_stats.return_value = {"x509_ca": 1}
        with mock.patch.dict(os.environ, {}, clear=True):
            self.assertFalse(
                shipyard_github_app_token.default_trust_store_is_missing(context)
            )

    def test_platform_ca_fallback_fails_closed_without_bundle(self) -> None:
        context = mock.Mock()

        with mock.patch.object(
            shipyard_github_app_token,
            "SYSTEM_CA_FILES",
            (),
        ):
            with self.assertRaises(shipyard_github_app_token.HelperError) as ctx:
                shipyard_github_app_token.add_platform_ca_files(context)

        self.assertIn("no platform CA bundle", str(ctx.exception))

    def test_api_request_retries_certificate_failure_with_augmented_context(
        self,
    ) -> None:
        context = mock.Mock()
        context.cert_store_stats.return_value = {"x509_ca": 0}
        response = mock.MagicMock()
        response.__enter__.return_value.read.return_value = b'{"id":123}'
        certificate_error = shipyard_github_app_token.urllib.error.URLError(
            shipyard_github_app_token.ssl.SSLCertVerificationError(
                1,
                "certificate verify failed",
            )
        )
        request = shipyard_github_app_token.urllib.request.Request(
            "https://api.github.com/test"
        )

        with mock.patch.object(
            shipyard_github_app_token.ssl,
            "create_default_context",
            return_value=context,
        ), mock.patch.object(
            shipyard_github_app_token,
            "default_trust_store_is_missing",
            return_value=True,
        ), mock.patch.object(
            shipyard_github_app_token,
            "add_platform_ca_files",
            return_value=context,
        ) as add_platform_ca, mock.patch.object(
            shipyard_github_app_token.urllib.request,
            "urlopen",
            side_effect=[certificate_error, response],
        ) as urlopen:
            payload = shipyard_github_app_token.api_request(request, "jwt")

        self.assertEqual(payload, {"id": 123})
        add_platform_ca.assert_called_once_with(context)
        self.assertEqual(urlopen.call_count, 2)

    def test_api_request_preserves_explicit_trust_policy_failure(self) -> None:
        context = mock.Mock()
        certificate_error = shipyard_github_app_token.urllib.error.URLError(
            shipyard_github_app_token.ssl.SSLCertVerificationError(
                1,
                "certificate verify failed",
            )
        )
        request = shipyard_github_app_token.urllib.request.Request(
            "https://github.example.test"
        )

        with mock.patch.object(
            shipyard_github_app_token.ssl,
            "create_default_context",
            return_value=context,
        ), mock.patch.object(
            shipyard_github_app_token,
            "default_trust_store_is_missing",
            return_value=False,
        ), mock.patch.object(
            shipyard_github_app_token,
            "add_platform_ca_files",
        ) as add_platform_ca, mock.patch.object(
            shipyard_github_app_token.urllib.request,
            "urlopen",
            side_effect=certificate_error,
        ):
            with self.assertRaises(shipyard_github_app_token.HelperError):
                shipyard_github_app_token.api_request(request, "jwt")

        add_platform_ca.assert_not_called()

    def test_api_request_retries_transient_service_failure(self) -> None:
        request = shipyard_github_app_token.urllib.request.Request(
            "https://api.github.com/test"
        )
        unavailable = shipyard_github_app_token.urllib.error.HTTPError(
            request.full_url,
            503,
            "Service Unavailable",
            {},
            io.BytesIO(b'{"message":"temporarily unavailable"}'),
        )
        response = mock.MagicMock()
        response.__enter__.return_value.read.return_value = b'{"id":123}'

        with mock.patch.object(
            shipyard_github_app_token.urllib.request,
            "urlopen",
            side_effect=[unavailable, response],
        ) as urlopen, mock.patch.object(
            shipyard_github_app_token.time,
            "sleep",
        ) as sleep:
            payload = shipyard_github_app_token.api_request(request, "jwt")

        self.assertEqual(payload, {"id": 123})
        self.assertEqual(urlopen.call_count, 2)
        sleep.assert_called_once_with(1)

    def test_api_request_does_not_retry_permission_failure(self) -> None:
        request = shipyard_github_app_token.urllib.request.Request(
            "https://api.github.com/test"
        )
        forbidden = shipyard_github_app_token.urllib.error.HTTPError(
            request.full_url,
            403,
            "Forbidden",
            {},
            io.BytesIO(b'{"message":"Resource not accessible by integration"}'),
        )

        with mock.patch.object(
            shipyard_github_app_token.urllib.request,
            "urlopen",
            side_effect=forbidden,
        ), mock.patch.object(
            shipyard_github_app_token.time,
            "sleep",
        ) as sleep:
            with self.assertRaises(shipyard_github_app_token.HelperError) as ctx:
                shipyard_github_app_token.api_request(request, "jwt")

        self.assertIn("HTTP 403", str(ctx.exception))
        sleep.assert_not_called()

    def test_main_outputs_installation_token_json(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            key_path = Path(temp) / "app.pem"
            key_path.write_text("fake-key", encoding="utf-8")
            if os.name != "nt":
                key_path.chmod(0o600)
            stdout = StringIO()
            argv = [
                "shipyard-github-app-token",
                "--app-id",
                "123",
                "--installation-id",
                "456",
                "--private-key",
                str(key_path),
            ]
            with mock.patch("sys.argv", argv), \
                    mock.patch.object(
                        shipyard_github_app_token,
                        "build_jwt",
                        return_value="jwt",
                    ), \
                    mock.patch.object(
                        shipyard_github_app_token,
                        "create_installation_token",
                        return_value={
                            "token": "ghs_test",
                            "expires_at": "2026-05-27T20:12:00Z",
                        },
                    ) as create_token, \
                    redirect_stdout(stdout):
                code = shipyard_github_app_token.main()

        self.assertEqual(code, 0)
        create_token.assert_called_once_with("https://api.github.com", "jwt", "456")
        payload = json.loads(stdout.getvalue())
        self.assertEqual(payload["token"], "ghs_test")
        self.assertEqual(payload["kind"], "github-app-installation")
        self.assertEqual(payload["expires_at"], "2026-05-27T20:12:00Z")

    def test_main_rejects_control_character_token_without_echoing_it(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            key_path = Path(temp) / "app.pem"
            key_path.write_text("fake-key", encoding="utf-8")
            if os.name != "nt":
                key_path.chmod(0o600)
            stdout = StringIO()
            argv = [
                "shipyard-github-app-token",
                "--app-id",
                "123",
                "--installation-id",
                "456",
                "--private-key",
                str(key_path),
            ]
            stderr = StringIO()
            with mock.patch("sys.argv", argv), mock.patch.object(
                shipyard_github_app_token,
                "build_jwt",
                return_value="jwt",
            ), mock.patch.object(
                shipyard_github_app_token,
                "create_installation_token",
                return_value={"token": "ghs_secret\nleak"},
            ), redirect_stdout(stdout), redirect_stderr(stderr):
                code = shipyard_github_app_token.main()

        self.assertEqual(code, 1)
        self.assertNotIn("ghs_secret", stdout.getvalue() + stderr.getvalue())

    def test_main_preserves_stable_writer_domain_overlap_exit(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            key_path = Path(temp) / "app.pem"
            key_path.write_text("fake-key", encoding="utf-8")
            if os.name != "nt":
                key_path.chmod(0o600)
            argv = [
                "shipyard-github-app-token",
                "--app-id",
                "123",
                "--installation-id",
                "456",
                "--private-key",
                str(key_path),
                "--cache-dir",
                str(Path(temp) / "cache"),
            ]
            stderr = StringIO()
            with mock.patch("sys.argv", argv), mock.patch.object(
                shipyard_github_app_token,
                "build_jwt",
                return_value="jwt",
            ), mock.patch.object(
                shipyard_github_app_token,
                "create_installation_token",
                return_value={
                    "token": "ghs_test",
                    "expires_at": "2026-05-27T20:12:00Z",
                },
            ), mock.patch.object(
                shipyard_github_app_token,
                "store_cached_token",
                side_effect=shipyard_github_app_token.HelperError(
                    "sandbox_writer_domain_overlap: exclusive sandbox audit owns lock"
                ),
            ), redirect_stderr(stderr):
                code = shipyard_github_app_token.main()

        self.assertEqual(code, 75)
        self.assertIn("sandbox_writer_domain_overlap:", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()

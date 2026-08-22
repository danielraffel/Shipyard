#!/usr/bin/env python3
from __future__ import annotations

import importlib.machinery
import importlib.util
import json
import os
import tempfile
import unittest
from contextlib import redirect_stdout
from io import StringIO
from pathlib import Path
from unittest import mock


SCRIPT_PATH = Path(__file__).with_name("shipyard-github-app-token")
LOADER = importlib.machinery.SourceFileLoader("shipyard_github_app_token", str(SCRIPT_PATH))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
assert SPEC is not None
shipyard_github_app_token = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(shipyard_github_app_token)


class GitHubAppTokenHelperTests(unittest.TestCase):
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

    def test_lookup_repo_installation_requires_repo_slug(self) -> None:
        with self.assertRaises(shipyard_github_app_token.HelperError) as ctx:
            shipyard_github_app_token.lookup_repo_installation(
                "https://api.github.com",
                "jwt",
                "not-a-slug",
            )

        self.assertIn("owner/name", str(ctx.exception))

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

    def test_main_outputs_installation_token_json(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            key_path = Path(temp) / "app.pem"
            key_path.write_text("fake-key", encoding="utf-8")
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


if __name__ == "__main__":
    unittest.main()

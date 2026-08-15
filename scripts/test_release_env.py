#!/usr/bin/env python3
from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

import release_env


class ReleaseEnvTests(unittest.TestCase):
    def test_parse_dotenv_strips_export_and_quotes(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / ".env"
            path.write_text(
                "\n".join(
                    [
                        "export APPLE_ID='dev@example.com'",
                        'TEAM_ID="TEAM123"',
                        "APP_SPECIFIC_PASSWORD=not-rendered",
                        "APP_CERT=Developer ID Application: Example (TEAM123)",
                    ]
                ),
                encoding="utf-8",
            )

            values = release_env.parse_dotenv(path)

        self.assertEqual(values["APPLE_ID"], "dev@example.com")
        self.assertEqual(values["TEAM_ID"], "TEAM123")
        self.assertEqual(values["APP_SPECIFIC_PASSWORD"], "not-rendered")
        self.assertEqual(
            values["APP_CERT"],
            "Developer ID Application: Example (TEAM123)",
        )

    def test_apply_dotenv_aliases_maps_plundertube_style_names(self) -> None:
        dotenv = {
            "APPLE_ID": "dev@example.com",
            "TEAM_ID": "TEAM123",
            "APP_SPECIFIC_PASSWORD": "password",
            "APP_CERT": "Developer ID Application: Example (TEAM123)",
            "GITHUB_PAT_PUBLIC_PUBLISHING": "do-not-map",
        }

        environ, sources = release_env.apply_dotenv_aliases({}, dotenv)

        self.assertEqual(environ["SHIPYARD_NOTARIZE_APPLE_ID"], "dev@example.com")
        self.assertEqual(environ["SHIPYARD_NOTARIZE_TEAM_ID"], "TEAM123")
        self.assertEqual(environ["SHIPYARD_NOTARIZE_APP_PASSWORD"], "password")
        self.assertEqual(
            environ["SHIPYARD_SIGNING_IDENTITY"],
            "Developer ID Application: Example (TEAM123)",
        )
        self.assertNotIn("RELEASE_BOT_TOKEN", environ)
        self.assertEqual(
            sources["SHIPYARD_NOTARIZE_APP_PASSWORD"],
            "dotenv:APP_SPECIFIC_PASSWORD",
        )

    def test_shipyard_prefixed_environment_wins_over_aliases(self) -> None:
        environ = {"SHIPYARD_NOTARIZE_APPLE_ID": "shipyard@example.com"}
        dotenv = {"APPLE_ID": "other@example.com"}

        merged, sources = release_env.apply_dotenv_aliases(environ, dotenv)

        self.assertEqual(merged["SHIPYARD_NOTARIZE_APPLE_ID"], "shipyard@example.com")
        self.assertEqual(sources["SHIPYARD_NOTARIZE_APPLE_ID"], "environment")

    def test_apply_dotenv_aliases_maps_m5_unattended_files(self) -> None:
        dotenv = {
            "PULP_SIGN_IDENTITY_HASH": "identity-hash",
            "PULP_SIGN_KEYCHAIN": "/tmp/pulp-signing.keychain-db",
            "PULP_SIGN_P12": "/tmp/signing.p12",
            "PULP_SIGN_P12_PW": "p12-secret",
            "PULP_NOTARY_KEY_PATH": "/tmp/AuthKey_TEST.p8",
            "PULP_NOTARY_KEY_ID": "KEY123",
            "PULP_NOTARY_ISSUER_ID": "issuer-uuid",
        }

        environ, sources = release_env.apply_dotenv_aliases({}, dotenv)

        self.assertEqual(environ["SHIPYARD_SIGNING_IDENTITY"], "identity-hash")
        self.assertEqual(
            environ["SHIPYARD_SIGNING_KEYCHAIN"],
            "/tmp/pulp-signing.keychain-db",
        )
        self.assertEqual(
            environ["SHIPYARD_NOTARIZE_KEY_PATH"],
            "/tmp/AuthKey_TEST.p8",
        )
        self.assertEqual(environ["SHIPYARD_SIGNING_P12"], "/tmp/signing.p12")
        self.assertEqual(environ["SHIPYARD_SIGNING_P12_PASSWORD"], "p12-secret")
        self.assertEqual(environ["SHIPYARD_NOTARIZE_KEY_ID"], "KEY123")
        self.assertEqual(environ["SHIPYARD_NOTARIZE_ISSUER_ID"], "issuer-uuid")
        self.assertEqual(
            sources["SHIPYARD_SIGNING_KEYCHAIN"],
            "dotenv:PULP_SIGN_KEYCHAIN",
        )


if __name__ == "__main__":
    unittest.main()

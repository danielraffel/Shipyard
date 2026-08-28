#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
ARTIFACT = "shipyard-linux-x64"


class InstallChecksumTests(unittest.TestCase):
    def run_installer(
        self,
        *,
        asset: bytes = b"verified shipyard binary\n",
        manifest: str | None = None,
        include_manifest_asset: bool = True,
        token: str | None = None,
    ) -> tuple[subprocess.CompletedProcess[str], Path, tempfile.TemporaryDirectory[str]]:
        temp = tempfile.TemporaryDirectory()
        root = Path(temp.name)
        fixture_asset = root / "asset"
        fixture_asset.write_bytes(asset)
        fixture_manifest = root / "checksums.sha256"
        if manifest is None:
            digest = hashlib.sha256(asset).hexdigest()
            manifest = f"{digest}  {ARTIFACT}\n"
        fixture_manifest.write_text(manifest, encoding="utf-8")

        assets = [
            {
                "name": ARTIFACT,
                "browser_download_url": "https://example.invalid/asset",
                "url": "https://api.example.invalid/asset",
            }
        ]
        if include_manifest_asset:
            assets.append(
                {
                    "name": "checksums.sha256",
                    "browser_download_url": "https://example.invalid/checksums.sha256",
                    "url": "https://api.example.invalid/checksums.sha256",
                }
            )
        release_json = root / "release.json"
        release_json.write_text(json.dumps({"assets": assets}), encoding="utf-8")

        fake_curl = root / "curl"
        fake_curl.write_text(
            """#!/usr/bin/env bash
set -euo pipefail
output=""
url=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) output="$2"; shift 2 ;;
        -H|-w) shift 2 ;;
        -*) shift ;;
        *) url="$1"; shift ;;
    esac
done
if [ -z "$output" ]; then
    cat "$FAKE_RELEASE_JSON"
    printf '\n200'
elif [[ "$url" == *checksums.sha256 ]]; then
    cp "$FAKE_CHECKSUMS" "$output"
else
    cp "$FAKE_ASSET" "$output"
fi
""",
            encoding="utf-8",
        )
        fake_curl.chmod(0o755)
        install_dir = root / "bin"
        env = os.environ.copy()
        env.update(
            {
                "FAKE_ASSET": str(fixture_asset),
                "FAKE_CHECKSUMS": str(fixture_manifest),
                "FAKE_RELEASE_JSON": str(release_json),
                "SHIPYARD_CURL_BIN": str(fake_curl),
                "SHIPYARD_INSTALL_DIR": str(install_dir),
                "SHIPYARD_INSTALL_TEST_UNAME_S": "Linux",
                "SHIPYARD_INSTALL_TEST_UNAME_M": "x86_64",
                "SHIPYARD_SKIP_SMOKE": "1",
                "SHIPYARD_VERSION": "v1.2.3",
            }
        )
        if token is not None:
            env["SHIPYARD_GITHUB_TOKEN"] = token
        result = subprocess.run(
            ["bash", str(ROOT / "install.sh")],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )
        return result, install_dir / "shipyard", temp

    def test_installs_only_after_exact_checksum_match(self) -> None:
        result, destination, temp = self.run_installer()
        try:
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(destination.read_bytes(), b"verified shipyard binary\n")
        finally:
            temp.cleanup()

    def test_refuses_checksum_mismatch_without_installing(self) -> None:
        result, destination, temp = self.run_installer(
            manifest=f"{'0' * 64}  {ARTIFACT}\n"
        )
        try:
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("SHA-256 verification failed", result.stderr)
            self.assertFalse(destination.exists())
        finally:
            temp.cleanup()

    def test_refuses_missing_exact_filename_entry(self) -> None:
        digest = hashlib.sha256(b"verified shipyard binary\n").hexdigest()
        result, destination, temp = self.run_installer(
            manifest=f"{digest}  prefix-{ARTIFACT}\n"
        )
        try:
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("exactly one valid entry", result.stderr)
            self.assertFalse(destination.exists())
        finally:
            temp.cleanup()

    def test_refuses_duplicate_exact_filename_entries(self) -> None:
        digest = hashlib.sha256(b"verified shipyard binary\n").hexdigest()
        result, destination, temp = self.run_installer(
            manifest=f"{digest}  {ARTIFACT}\n{digest}  {ARTIFACT}\n"
        )
        try:
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("exactly one valid entry", result.stderr)
            self.assertFalse(destination.exists())
        finally:
            temp.cleanup()

    def test_refuses_malformed_digest(self) -> None:
        result, destination, temp = self.run_installer(
            manifest=f"not-a-sha256  {ARTIFACT}\n"
        )
        try:
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("exactly one valid entry", result.stderr)
            self.assertFalse(destination.exists())
        finally:
            temp.cleanup()

    def test_refuses_release_without_checksum_asset(self) -> None:
        result, destination, temp = self.run_installer(include_manifest_asset=False)
        try:
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("has no checksums.sha256 asset", result.stderr)
            self.assertFalse(destination.exists())
        finally:
            temp.cleanup()

    def test_token_is_not_printed_when_verification_fails(self) -> None:
        token = "private-token-must-not-print"
        result, _destination, temp = self.run_installer(
            manifest=f"{'0' * 64}  {ARTIFACT}\n", token=token
        )
        try:
            self.assertNotIn(token, result.stdout)
            self.assertNotIn(token, result.stderr)
        finally:
            temp.cleanup()


if __name__ == "__main__":
    unittest.main()

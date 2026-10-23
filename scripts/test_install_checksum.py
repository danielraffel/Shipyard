#!/usr/bin/env python3
"""install.sh verifies checksums.sha256 before it signs or runs anything.

Every test drives the real install.sh against a local fixture release. The
`curl` it runs (SHIPYARD_CURL_BIN) is a shim that serves files from a fixture
directory and fails loudly on any URL it does not know, so no test touches the
network. On the macOS DMG path `hdiutil`, `codesign` and `xattr` are shims too,
which record every call: a tampered asset must be refused before any of them
(or the downloaded binary itself) runs.
"""
from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
INSTALL_SH = ROOT / "install.sh"
VERSION = "0.200.0"
TAG = f"v{VERSION}"
BASE_URL = "https://fixture.invalid/download"

FAKE_CURL = r'''
import json
import os
import sys
from pathlib import Path

served = Path(os.environ["FIXTURE_SERVED"])
with open(os.environ["FIXTURE_CURL_LOG"], "a", encoding="utf-8") as log:
    log.write(json.dumps(sys.argv[1:]) + "\n")
args = sys.argv[1:]
url = next((a for a in args if a.startswith("https://")), None)
output = args[args.index("-o") + 1] if "-o" in args else None
routes = json.loads((served / "routes.json").read_text(encoding="utf-8"))
if url not in routes:
    sys.stderr.write(f"fake curl: unexpected URL {url!r}\n")
    sys.exit(97)
body = (served / routes[url]).read_bytes()
if output:
    Path(output).write_bytes(body)
else:
    sys.stdout.buffer.write(body)
if "-w" in args:
    sys.stdout.write("\n200")
'''

RECORDING_TOOL = """#!/bin/sh
printf '%s %s\\n' "$(basename "$0")" "$*" >> "$FIXTURE_TOOL_LOG"
exit 0
"""

# Shim hdiutil: "mounting" copies the fixture DMG payload into the mountpoint.
FAKE_HDIUTIL = """#!/bin/sh
printf 'hdiutil %s\\n' "$*" >> "$FIXTURE_TOOL_LOG"
if [ "$1" = "attach" ]; then
    while [ "$#" -gt 0 ]; do
        if [ "$1" = "-mountpoint" ]; then mkdir -p "$2"; cp "$FIXTURE_DMG_PAYLOAD"/* "$2"/; fi
        shift
    done
fi
exit 0
"""


def fake_binary(name: str, marker: Path) -> bytes:
    return (
        f"#!/bin/sh\necho ran >> '{marker}'\necho '{name} {VERSION}'\n"
    ).encode()


class InstallChecksumTests(unittest.TestCase):
    def setUp(self) -> None:
        self._temp = tempfile.TemporaryDirectory()
        self.root = Path(self._temp.name)
        self.served = self.root / "served"
        self.served.mkdir()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.install_dir = self.root / "install"
        self.ran_marker = self.root / "binary-ran"
        self.curl_log = self.root / "curl.log"
        self.tool_log = self.root / "tools.log"
        self.fake_curl = self.root / "fake-curl"
        self.fake_curl.write_text(
            f"#!/bin/sh\nexec '{sys.executable}' -I '{self.root / 'fake_curl.py'}' \"$@\"\n",
            encoding="utf-8",
        )
        self.fake_curl.chmod(0o755)
        (self.root / "fake_curl.py").write_text(FAKE_CURL, encoding="utf-8")

    def tearDown(self) -> None:
        self._temp.cleanup()

    # ── fixture release ──────────────────────────────────────────────

    def publish(
        self,
        assets: dict[str, bytes],
        *,
        checksums: bool = True,
        tamper: str | None = None,
        omit_entry: str | None = None,
    ) -> None:
        """Serve a release. Checksums are taken BEFORE `tamper` is altered."""
        lines = [
            f"{hashlib.sha256(body).hexdigest()}  {name}"
            for name, body in sorted(assets.items())
            if name != omit_entry
        ]
        routes = {}
        release_assets = []
        served_assets = dict(assets)
        if tamper:
            served_assets[tamper] = served_assets[tamper] + b"# tampered\n"
        if checksums:
            served_assets["checksums.sha256"] = ("\n".join(lines) + "\n").encode()
        for name, body in served_assets.items():
            (self.served / name).write_bytes(body)
            url = f"{BASE_URL}/{TAG}/{name}"
            routes[url] = name
            release_assets.append(
                {"name": name, "browser_download_url": url, "url": url}
            )
        (self.served / "release.json").write_text(
            json.dumps({"tag_name": TAG, "assets": release_assets}), encoding="utf-8"
        )
        routes[
            f"https://api.github.com/repos/danielraffel/Shipyard/releases/tags/{TAG}"
        ] = "release.json"
        (self.served / "routes.json").write_text(json.dumps(routes), encoding="utf-8")

    def linux_assets(self) -> dict[str, bytes]:
        return {
            "shipyard-linux-x64": fake_binary("shipyard", self.ran_marker),
            "shipyard-workstream-provider-linux-x64": fake_binary(
                "shipyard-workstream-provider", self.ran_marker
            ),
        }

    def macos_dmg(self) -> dict[str, bytes]:
        payload = self.root / "dmg-payload"
        payload.mkdir(exist_ok=True)
        for name in ("shipyard", "shipyard-workstream-provider"):
            binary = payload / name
            binary.write_bytes(fake_binary(name, self.ran_marker))
            binary.chmod(0o755)
        return {"shipyard-macos-arm64.dmg": b"fixture dmg image bytes\n"}

    def install_macos_shims(self) -> None:
        for name in ("codesign", "xattr"):
            tool = self.bin / name
            tool.write_text(RECORDING_TOOL, encoding="utf-8")
            tool.chmod(0o755)
        hdiutil = self.bin / "hdiutil"
        hdiutil.write_text(FAKE_HDIUTIL, encoding="utf-8")
        hdiutil.chmod(0o755)

    def run_install(
        self, uname_s: str, uname_m: str, extra_env: dict[str, str] | None = None
    ) -> subprocess.CompletedProcess:
        env = {
            "HOME": str(self.root / "home"),
            "PATH": f"{self.bin}:/usr/bin:/bin:/usr/sbin:/sbin",
            "SHIPYARD_VERSION": TAG,
            "SHIPYARD_INSTALL_DIR": str(self.install_dir),
            "SHIPYARD_CURL_BIN": str(self.fake_curl),
            "SHIPYARD_INSTALL_TEST_UNAME_S": uname_s,
            "SHIPYARD_INSTALL_TEST_UNAME_M": uname_m,
            "FIXTURE_SERVED": str(self.served),
            "FIXTURE_CURL_LOG": str(self.curl_log),
            "FIXTURE_TOOL_LOG": str(self.tool_log),
            "FIXTURE_DMG_PAYLOAD": str(self.root / "dmg-payload"),
            **(extra_env or {}),
        }
        return subprocess.run(
            ["bash", str(INSTALL_SH)],
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
        )

    def tool_calls(self) -> str:
        return self.tool_log.read_text(encoding="utf-8") if self.tool_log.exists() else ""

    def assert_nothing_installed(self) -> None:
        self.assertFalse((self.install_dir / "shipyard").exists())
        self.assertFalse((self.install_dir / "shipyard-workstream-provider").exists())
        leftovers = [p.name for p in self.install_dir.iterdir()] if self.install_dir.exists() else []
        self.assertEqual(leftovers, [], "staged downloads must be cleaned up")
        self.assertFalse(self.ran_marker.exists(), "unverified binary was executed")

    # ── raw binary path (Linux / Windows) ────────────────────────────

    def test_untouched_linux_assets_install(self) -> None:
        self.publish(self.linux_assets())
        result = self.run_install("Linux", "x86_64")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Verified sha256 for shipyard-linux-x64.", result.stdout)
        self.assertIn(
            "Verified sha256 for shipyard-workstream-provider-linux-x64.", result.stdout
        )
        self.assertTrue((self.install_dir / "shipyard").exists())
        self.assertTrue((self.install_dir / "shipyard-workstream-provider").exists())
        fetched = self.curl_log.read_text(encoding="utf-8")
        self.assertIn("checksums.sha256", fetched)

    def test_tampered_cli_is_refused_before_it_runs(self) -> None:
        self.publish(self.linux_assets(), tamper="shipyard-linux-x64")
        result = self.run_install("Linux", "x86_64")
        self.assertEqual(result.returncode, 1)
        self.assertIn("sha256 mismatch for shipyard-linux-x64", result.stderr)
        self.assert_nothing_installed()

    def test_tampered_provider_is_refused_before_it_runs(self) -> None:
        self.publish(
            self.linux_assets(), tamper="shipyard-workstream-provider-linux-x64"
        )
        result = self.run_install("Linux", "x86_64")
        self.assertEqual(result.returncode, 1)
        self.assertIn(
            "sha256 mismatch for shipyard-workstream-provider-linux-x64", result.stderr
        )
        self.assert_nothing_installed()

    def test_checksums_without_an_entry_for_the_asset_is_refused(self) -> None:
        self.publish(self.linux_assets(), omit_entry="shipyard-linux-x64")
        result = self.run_install("Linux", "x86_64")
        self.assertEqual(result.returncode, 1)
        self.assertIn("has no entry for shipyard-linux-x64", result.stderr)
        self.assert_nothing_installed()

    def test_release_without_checksums_is_refused(self) -> None:
        self.publish(self.linux_assets(), checksums=False)
        result = self.run_install("Linux", "x86_64")
        self.assertEqual(result.returncode, 1)
        self.assertIn("publishes no checksums.sha256", result.stderr)
        self.assertIn("SHIPYARD_ALLOW_UNVERIFIED_INSTALL=1", result.stderr)
        fetched = self.curl_log.read_text(encoding="utf-8")
        self.assertNotIn("shipyard-linux-x64", fetched, "nothing may be downloaded")
        self.assert_nothing_installed()

    def test_unreachable_checksums_is_refused_even_with_escape_hatch(self) -> None:
        self.publish(self.linux_assets())
        (self.served / "checksums.sha256").write_bytes(b"")
        result = self.run_install(
            "Linux", "x86_64", {"SHIPYARD_ALLOW_UNVERIFIED_INSTALL": "1"}
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("failed to download checksums.sha256", result.stderr)
        self.assert_nothing_installed()

    def test_escape_hatch_installs_unverified_release_loudly(self) -> None:
        self.publish(self.linux_assets(), checksums=False)
        result = self.run_install(
            "Linux", "x86_64", {"SHIPYARD_ALLOW_UNVERIFIED_INSTALL": "1"}
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("WITHOUT checksum verification", result.stderr)
        self.assertIn("SHIPYARD_ALLOW_UNVERIFIED_INSTALL=1 is set", result.stderr)
        self.assertNotIn("Verified sha256", result.stdout)
        self.assertTrue((self.install_dir / "shipyard").exists())

    def test_escape_hatch_does_not_bypass_a_mismatch(self) -> None:
        self.publish(self.linux_assets(), tamper="shipyard-linux-x64")
        result = self.run_install(
            "Linux", "x86_64", {"SHIPYARD_ALLOW_UNVERIFIED_INSTALL": "1"}
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("sha256 mismatch for shipyard-linux-x64", result.stderr)
        self.assert_nothing_installed()

    # ── macOS DMG path ───────────────────────────────────────────────

    def test_untouched_dmg_is_verified_then_mounted_and_signed(self) -> None:
        self.install_macos_shims()
        self.publish(self.macos_dmg())
        result = self.run_install("Darwin", "arm64")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Verified sha256 for shipyard-macos-arm64.dmg.", result.stdout)
        calls = self.tool_calls()
        self.assertIn("hdiutil attach", calls)
        self.assertIn("codesign", calls)
        self.assertTrue((self.install_dir / "shipyard").exists())

    def test_dmg_release_without_checksums_is_refused_before_download(self) -> None:
        self.install_macos_shims()
        self.publish(self.macos_dmg(), checksums=False)
        result = self.run_install("Darwin", "arm64")
        self.assertEqual(result.returncode, 1)
        self.assertIn("publishes no checksums.sha256", result.stderr)
        self.assertEqual(self.tool_calls(), "", "no hdiutil/codesign/xattr may run")
        self.assert_nothing_installed()

    def test_tampered_dmg_is_refused_before_mount_or_signing(self) -> None:
        self.install_macos_shims()
        self.publish(self.macos_dmg(), tamper="shipyard-macos-arm64.dmg")
        result = self.run_install("Darwin", "arm64")
        self.assertEqual(result.returncode, 1)
        self.assertIn("sha256 mismatch for shipyard-macos-arm64.dmg", result.stderr)
        self.assertEqual(self.tool_calls(), "", "no hdiutil/codesign/xattr may run")
        self.assert_nothing_installed()


if __name__ == "__main__":
    unittest.main()

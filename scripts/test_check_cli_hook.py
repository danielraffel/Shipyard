#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


class CheckCliHookTests(unittest.TestCase):
    def test_marketplace_plugin_version_matches_plugin_manifest(self) -> None:
        plugin = json.loads(
            (ROOT / ".claude-plugin" / "plugin.json").read_text(encoding="utf-8")
        )
        marketplace = json.loads(
            (ROOT / ".claude-plugin" / "marketplace.json").read_text(
                encoding="utf-8"
            )
        )
        entry = next(
            item for item in marketplace["plugins"] if item["name"] == "shipyard"
        )
        self.assertEqual(entry["version"], plugin["version"])

    def test_partial_install_warns_without_running_installer(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            shipyard = bin_dir / "shipyard"
            shipyard.write_text(
                "#!/bin/sh\necho 'shipyard 0.127.0'\n", encoding="utf-8"
            )
            shipyard.chmod(0o755)
            curl_marker = root / "curl-ran"
            curl = bin_dir / "curl"
            curl.write_text(
                f"#!/bin/sh\ntouch '{curl_marker}'\nexit 99\n", encoding="utf-8"
            )
            curl.chmod(0o755)
            env = {
                "HOME": str(root / "home"),
                "PATH": f"{bin_dir}:/usr/bin:/bin:/usr/sbin:/sbin",
                "CLAUDE_PLUGIN_ROOT": str(ROOT),
            }

            result = subprocess.run(
                ["bash", str(ROOT / "hooks" / "check-cli.sh")],
                env=env,
                check=True,
                capture_output=True,
                text=True,
            )

            self.assertIn("shipyard-workstream-provider is missing", result.stdout)
            self.assertIn("will not surprise-upgrade", result.stdout)
            self.assertFalse(curl_marker.exists())

    def test_valid_same_directory_pair_does_not_warn(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            for name in ("shipyard", "shipyard-workstream-provider"):
                binary = bin_dir / name
                binary.write_text(
                    f"#!/bin/sh\necho '{name} 0.127.1'\n", encoding="utf-8"
                )
                binary.chmod(0o755)

            result = subprocess.run(
                ["bash", str(ROOT / "hooks" / "check-cli.sh")],
                env={
                    "HOME": str(root / "home"),
                    "PATH": f"{bin_dir}:/usr/bin:/bin:/usr/sbin:/sbin",
                    "CLAUDE_PLUGIN_ROOT": str(ROOT),
                },
                check=True,
                capture_output=True,
                text=True,
            )

            self.assertNotIn("Installation is incomplete", result.stdout)

    def test_pre_provider_pin_is_not_reported_as_a_partial_install(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            shipyard = bin_dir / "shipyard"
            shipyard.write_text(
                "#!/bin/sh\necho 'shipyard 0.126.2'\n", encoding="utf-8"
            )
            shipyard.chmod(0o755)
            env = {
                "HOME": str(root / "home"),
                "PATH": f"{bin_dir}:/usr/bin:/bin:/usr/sbin:/sbin",
                "CLAUDE_PLUGIN_ROOT": str(ROOT),
            }

            result = subprocess.run(
                ["bash", str(ROOT / "hooks" / "check-cli.sh")],
                env=env,
                check=True,
                capture_output=True,
                text=True,
            )

            self.assertNotIn("Installation is incomplete", result.stdout)

    def test_companion_must_be_same_dir_launchable_and_same_version(self) -> None:
        scenarios = (
            ("other-path", None, "0.127.0", True),
            ("not-launchable", "exit 9", None, False),
            ("version-mismatch", "echo 'shipyard-workstream-provider 0.127.1'", None, True),
        )
        for name, same_dir_body, other_version, executable in scenarios:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                bin_dir = root / "bin"
                other_bin = root / "other-bin"
                bin_dir.mkdir()
                other_bin.mkdir()
                shipyard = bin_dir / "shipyard"
                shipyard.write_text(
                    "#!/bin/sh\necho 'shipyard 0.127.0'\n", encoding="utf-8"
                )
                shipyard.chmod(0o755)
                provider_dir = other_bin if other_version else bin_dir
                provider = provider_dir / "shipyard-workstream-provider"
                body = same_dir_body or (
                    f"echo 'shipyard-workstream-provider {other_version}'"
                )
                provider.write_text(f"#!/bin/sh\n{body}\n", encoding="utf-8")
                provider.chmod(0o755 if executable else 0o644)
                curl_marker = root / "curl-ran"
                curl = bin_dir / "curl"
                curl.write_text(
                    f"#!/bin/sh\ntouch '{curl_marker}'\nexit 99\n",
                    encoding="utf-8",
                )
                curl.chmod(0o755)

                result = subprocess.run(
                    ["bash", str(ROOT / "hooks" / "check-cli.sh")],
                    env={
                        "HOME": str(root / "home"),
                        "PATH": f"{bin_dir}:{other_bin}:/usr/bin:/bin:/usr/sbin:/sbin",
                        "CLAUDE_PLUGIN_ROOT": str(ROOT),
                    },
                    check=True,
                    capture_output=True,
                    text=True,
                )

                self.assertIn("Installation is incomplete", result.stdout)
                self.assertIn("will not surprise-upgrade", result.stdout)
                self.assertFalse(curl_marker.exists())


if __name__ == "__main__":
    unittest.main()

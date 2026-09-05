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

    def test_marketplace_is_registered_as_a_plugin_version_file(self) -> None:
        """The bump tool must actually own marketplace.json, not just be able to.

        The drift test above only fires *after* the two files have already
        diverged on main, and only on the next run of a check that does not
        block a merge. This asserts the arming instead: that the plugin
        surface lists marketplace.json among its version files, so
        `version_bump_check.py --mode=apply` moves both in lockstep.

        It was unregistered for long enough to ship a split-brain version,
        while the apply logic that keeps multiple files in step already
        existed and named this very file in its comments. A guard that is
        present but unwired is not a guard.
        """
        versioning = json.loads(
            (ROOT / "scripts" / "versioning.json").read_text(encoding="utf-8")
        )
        paths = [
            version_file["path"]
            for version_file in versioning["surfaces"]["plugin"]["version_files"]
        ]
        self.assertIn(".claude-plugin/plugin.json", paths)
        self.assertIn(".claude-plugin/marketplace.json", paths)

    def test_marketplace_version_pattern_targets_the_plugin_not_the_metadata(
        self,
    ) -> None:
        """marketplace.json carries two unrelated versions; pick the right one.

        `metadata.version` describes the marketplace itself and must never be
        rewritten by a plugin bump, so the pattern is anchored inside the
        `plugins` array. This is the planted control for that anchoring: it
        rewrites through the configured pattern and asserts the other version
        is left alone. An unanchored pattern passes the drift test above and
        silently corrupts the marketplace version instead.
        """
        import re

        versioning = json.loads(
            (ROOT / "scripts" / "versioning.json").read_text(encoding="utf-8")
        )
        version_file = next(
            candidate
            for candidate in versioning["surfaces"]["plugin"]["version_files"]
            if candidate["path"] == ".claude-plugin/marketplace.json"
        )
        text = (ROOT / ".claude-plugin" / "marketplace.json").read_text(
            encoding="utf-8"
        )
        match = re.search(version_file["pattern"], text)
        self.assertIsNotNone(match, "pattern must match marketplace.json")
        assert match is not None
        original = json.loads(text)
        self.assertEqual(
            match.group(1),
            next(
                item
                for item in original["plugins"]
                if item["name"] == "shipyard"
            )["version"],
            "the pattern must capture the plugin entry's version",
        )

        rewritten = json.loads(
            re.sub(
                version_file["pattern"],
                lambda found: found.group(0).replace(found.group(1), "9.9.9"),
                text,
                count=1,
            )
        )
        self.assertEqual(rewritten["plugins"][0]["version"], "9.9.9")
        self.assertEqual(
            rewritten["metadata"]["version"],
            original["metadata"]["version"],
            "a plugin bump must not rewrite the marketplace's own version",
        )

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

    def test_windows_hook_uses_same_directory_exe_companion(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            shipyard = bin_dir / "shipyard"
            shipyard.write_text(
                "#!/bin/sh\necho 'shipyard 0.127.1'\n", encoding="utf-8"
            )
            shipyard.chmod(0o755)
            provider = bin_dir / "shipyard-workstream-provider.exe"
            provider.write_text(
                "#!/bin/sh\necho 'shipyard-workstream-provider 0.127.1'\n",
                encoding="utf-8",
            )
            provider.chmod(0o755)
            uname = bin_dir / "uname"
            uname.write_text("#!/bin/sh\necho 'MSYS_NT-10.0'\n", encoding="utf-8")
            uname.chmod(0o755)

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

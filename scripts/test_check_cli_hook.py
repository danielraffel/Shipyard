#!/usr/bin/env python3
from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


class CheckCliHookTests(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()

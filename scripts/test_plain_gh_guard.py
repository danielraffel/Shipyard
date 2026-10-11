#!/usr/bin/env python3
"""Exercise the installed plain gh executable with the real queue guard."""
from __future__ import annotations
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ENTRY = ROOT / "scripts/gh"
INSTALL = ROOT / "scripts/install-guarded-gh.sh"
REAL_GUARD = ROOT / "scripts/ghapp_queue_removal_guard.py"


class PlainGhGuardTests(unittest.TestCase):
    def test_install_and_real_guard(self) -> None:
        with tempfile.TemporaryDirectory(prefix="shipyard-plain-gh-") as td:
            root = Path(td)
            home = root / "home"
            bin_dir = home / ".local/bin"
            guards_dir = home / ".config/shipyard/guards"
            native = root / "native-gh"
            native.write_text("#!/bin/sh\nprintf 'native:%s\\n' \"$*\"\n", encoding="utf-8")
            native.chmod(0o755)
            hostile_python = root / "python3"
            hostile_python.write_text("#!/bin/sh\nprintf hostile-python >&2\nexit 99\n", encoding="utf-8")
            hostile_python.chmod(0o755)
            env = os.environ.copy()
            env["HOME"] = str(home)
            env.pop("SHIPYARD_GHAPP_GUARDS_DIR", None)
            env.pop("SHIPYARD_NATIVE_GH_BINARY", None)
            env["SHIPYARD_GHAPP_GH_BINARY"] = str(native)
            install = subprocess.run([str(INSTALL), "--bin-dir", str(bin_dir)], env=env, text=True, encoding="utf-8", capture_output=True)
            self.assertEqual(install.returncode, 0, install.stderr)
            self.assertFalse((guards_dir / "queue-removal-guard").exists())
            self.assertIn("shipyard guards install", install.stdout)
            path_env = f"{bin_dir}:{root}:{env['PATH']}"
            resolved = subprocess.run(["bash", "-c", "command -v gh"], env={**env, "PATH": path_env}, text=True, encoding="utf-8", capture_output=True)
            self.assertEqual(Path(resolved.stdout.strip()).resolve(), (bin_dir / "gh").resolve(), resolved.stdout)
            guards_dir.mkdir(parents=True)
            shutil.copy2(REAL_GUARD, guards_dir / "queue-removal-guard")
            (guards_dir / "queue-removal-guard").chmod(0o755)
            harmless = subprocess.run([str(bin_dir / "gh"), "pr", "view", "1"], env={**env, "PATH": path_env}, text=True, encoding="utf-8", capture_output=True)
            self.assertEqual(harmless.returncode, 0, harmless)
            self.assertIn("native:pr view 1", harmless.stdout)
            blocked = subprocess.run([str(bin_dir / "gh"), "api", "graphql", "-f", "query=mutation { dequeuePullRequest(input:{id:\"x\"}) { clientMutationId } }"], env={**env, "PATH": path_env}, text=True, encoding="utf-8", capture_output=True)
            self.assertNotEqual(blocked.returncode, 0, blocked)
            self.assertIn("unaudited merge-queue removal", blocked.stderr)
            self.assertNotIn("native:", blocked.stdout)
            self.assertNotIn("hostile-python", blocked.stderr)

            (guards_dir / "queue-removal-guard").write_text(
                "#!/bin/sh\n"
                f"[ \"${{GHAPP_REAL_GH:-}}\" = \"{native}\" ] || exit 91\n",
                encoding="utf-8",
            )
            (guards_dir / "queue-removal-guard").chmod(0o755)
            exported = subprocess.run(
                [str(bin_dir / "gh"), "pr", "view", "1"],
                env={**env, "PATH": path_env},
                text=True,
                encoding="utf-8",
                capture_output=True,
            )
            self.assertEqual(exported.returncode, 0, exported)


if __name__ == "__main__":
    unittest.main()

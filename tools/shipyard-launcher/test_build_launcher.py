"""Tests for build_launcher.py. Only the dry run executes: nothing here signs."""

import pathlib
import plistlib
import shutil
import subprocess
import sys
import tempfile
import unittest

BUILD = pathlib.Path(__file__).resolve().with_name("build_launcher.py")
ON_MAC = sys.platform == "darwin" and shutil.which("xcrun") is not None


class BuildLauncherTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.output = pathlib.Path(self.tmp.name) / "out"

    def tearDown(self):
        self.tmp.cleanup()

    def build(self, *args):
        return subprocess.run(
            [sys.executable, str(BUILD), *args, "--output", str(self.output)],
            capture_output=True, text=True, timeout=600,
        )

    def test_a_mode_is_required_and_notarize_needs_sign(self):
        self.assertEqual(self.build().returncode, 2)
        result = self.build("--dry-run", "--notarize")
        self.assertEqual(result.returncode, 2)
        self.assertIn("--notarize needs --sign", result.stderr)

    @unittest.skipUnless(ON_MAC, "needs macOS with xcrun swiftc")
    def test_dry_run_builds_an_unsigned_bundle_and_prints_the_plan(self):
        result = self.build("--dry-run")
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        app = self.output / "ShipyardLauncher.app"
        info = plistlib.loads((app / "Contents" / "Info.plist").read_bytes())
        self.assertEqual(info["CFBundleIdentifier"], "com.danielraffel.shipyard.launcher")
        self.assertTrue(info["LSBackgroundOnly"])
        self.assertIn("NSRemovableVolumesUsageDescription", info)
        for step in ("--options runtime", "--identifier com.danielraffel.shipyard.launcher",
                     "notarytool submit", "stapler staple", "spctl --assess"):
            self.assertIn(step, result.stdout)
        self.assertIn("(unsigned)", result.stdout)
        # Nothing was signed with an identity, and no archive was produced.
        signature = subprocess.run(["codesign", "-dv", str(app)], capture_output=True, text=True)
        self.assertNotIn("Authority=", signature.stderr)
        self.assertFalse((self.output / "ShipyardLauncher-macos-arm64.zip").exists())
        # A second build never replaces a bundle in place.
        again = self.build("--dry-run")
        self.assertNotEqual(again.returncode, 0)
        self.assertIn("refusing to replace", again.stderr)


if __name__ == "__main__":
    unittest.main()

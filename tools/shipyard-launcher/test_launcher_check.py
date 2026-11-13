"""Tests for shipyard-launcher-check with fake codesign, spctl and open."""

import json
import os
import pathlib
import stat
import subprocess
import sys
import tempfile
import unittest

CHECK = pathlib.Path(__file__).resolve().with_name("shipyard-launcher-check")

FAKE_CODESIGN = """#!/bin/bash
case "$1" in
  -dv) case "${@: -1}" in
         *.app) echo "TeamIdentifier=${FAKE_BUNDLE_TEAM:-${FAKE_TEAM:-TEAM123456}}" >&2 ;;
         *) echo "TeamIdentifier=${FAKE_TEAM:-TEAM123456}" >&2 ;;
       esac; exit 0 ;;
  -dvvv) echo "TeamIdentifier=${FAKE_BUNDLE_TEAM:-${FAKE_TEAM:-TEAM123456}}" >&2; echo "CDHash=${FAKE_CDHASH:-0123456789abcdef0123456789abcdef01234567}" >&2; exit 0 ;;
  --verify)
    for arg; do case "$arg" in -R=*) echo "$arg" > "$FAKE_TOOLS/requirement" ;; esac; done
    [ -f "$FAKE_TOOLS/codesign-refuses" ] && { echo "a sealed resource is missing or invalid" >&2; exit 1; }
    exit 0 ;;
esac
exit 2
"""
FAKE_SPCTL = """#!/bin/bash
if [ -f "$FAKE_TOOLS/spctl-mentions" ]; then echo "accepted"; echo "source=Developer ID (not Notarized Developer ID)"; exit 0; fi
if [ -f "$FAKE_TOOLS/unnotarized" ]; then echo "rejected"; echo "source=Unnotarized Developer ID"; exit 3; fi
echo "accepted"; echo "source=Notarized Developer ID"
"""
# `open -n -g -a APP --args --launcher-check-access --launcher-result FILE`
FAKE_OPEN = """#!/bin/bash
[ -f "$FAKE_TOOLS/open-fails" ] && { echo "LSOpenURLsWithRole() failed with error -10810" >&2; exit 1; }
echo "$*" > "$FAKE_TOOLS/open-args"
result="${@: -1}"
printf '{"schema":1,"full_disk_access":"%s","errno":%s,"ppid":%s}\\n' "${FAKE_FDA:-denied}" "${FAKE_ERRNO:-1}" "${FAKE_PPID:-1}" > "$result"
"""
FAKE_LAUNCHER = """#!/bin/bash
if [ "$1" = --launcher-active ]; then
  record="$SHIPYARD_LAUNCHER_CHECK_HOME/.local/share/shipyard/launcher/activation.json"
  [ -f "$record" ] || { echo '{"active":false,"reason":"not_activated"}'; exit 1; }
  [ -f "$FAKE_TOOLS/launcher-inactive" ] && { echo '{"active":false,"reason":"bundle_replaced: code directory hash differs"}'; exit 1; }
  cp "$record" "$FAKE_TOOLS/seen-record"
  echo '{"active":true,"reason":"grant_cached"}'; exit 0
fi
[ "$1" = --launcher-verify ] || exit 64
[ -f "$FAKE_TOOLS/launcher-refuses" ] && { echo "shipyard-launcher: not a Developer ID signed Shipyard" >&2; exit 77; }
[ -f "$FAKE_TOOLS/launcher-says-not-ok" ] && { echo '{"ok":false}'; exit 0; }
echo '{"ok":true,"target":"/h/.local/share/shipyard/auth-generations/x/shipyard"}'
"""


class LauncherCheckTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.home = pathlib.Path(self.tmp.name)
        self.tools = self.home / "tools"
        self.tools.mkdir()
        for name, text in (("codesign", FAKE_CODESIGN), ("spctl", FAKE_SPCTL), ("open", FAKE_OPEN)):
            (self.tools / name).write_text(text)
            (self.tools / name).chmod(0o755)
        (self.home / ".local" / "bin").mkdir(parents=True)
        (self.home / ".local" / "bin" / "shipyard").write_text("#!/bin/sh\n")
        self.app = self.home / ".local" / "libexec" / "ShipyardLauncher.app"
        self.record = self.home / ".local" / "share" / "shipyard" / "launcher" / "activation.json"
        self.env = dict(
            os.environ,
            SHIPYARD_LAUNCHER_CHECK_HOME=str(self.home),
            SHIPYARD_LAUNCHER_CHECK_CODESIGN=str(self.tools / "codesign"),
            SHIPYARD_LAUNCHER_CHECK_SPCTL=str(self.tools / "spctl"),
            SHIPYARD_LAUNCHER_CHECK_OPEN=str(self.tools / "open"),
            SHIPYARD_LAUNCHER_CHECK_WAIT_SECS="3",
            FAKE_TOOLS=str(self.tools),
        )

    def tearDown(self):
        self.tmp.cleanup()

    def install(self):
        exe = self.app / "Contents" / "MacOS" / "shipyard-launcher"
        exe.parent.mkdir(parents=True)
        exe.write_text(FAKE_LAUNCHER)
        exe.chmod(0o755)

    def flag(self, name):
        (self.tools / name).write_text("")

    def check(self, *args, **env):
        merged = dict(self.env)
        merged.update(env)
        return subprocess.run([sys.executable, str(CHECK), *args], env=merged, capture_output=True, text=True)

    def test_not_installed(self):
        result = self.check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("FAIL bundle", result.stdout)
        self.assertIn("Install on a host", result.stdout)

    def test_requirement_pins_developer_id_identifier_and_the_shipyard_team(self):
        self.install()
        self.check("--json", FAKE_TEAM="ABCDE12345")
        requirement = (self.tools / "requirement").read_text()
        self.assertIn('identifier "com.danielraffel.shipyard.launcher"', requirement)
        self.assertIn("anchor apple generic", requirement)
        self.assertIn("certificate 1[field.1.2.840.113635.100.6.2.6] exists", requirement)
        self.assertIn("certificate leaf[field.1.2.840.113635.100.6.1.13] exists", requirement)
        self.assertIn('certificate leaf[subject.OU] = "ABCDE12345"', requirement)

    def test_bad_signature_or_missing_notarization_is_not_ready(self):
        self.install()
        self.flag("codesign-refuses")
        self.assertIn("signature does not satisfy", self.check().stdout)
        (self.tools / "codesign-refuses").unlink()
        self.flag("unnotarized")
        result = self.check("--verify-bundle", str(self.app))
        self.assertEqual(result.returncode, 1)
        self.assertIn("not notarized", result.stdout)

    def test_ambiguous_answers_are_never_a_pass(self):
        self.install()
        # A notarization source that only mentions the phrase is not one.
        self.flag("spctl-mentions")
        self.assertEqual(self.check("--verify-bundle", str(self.app)).returncode, 1)
        (self.tools / "spctl-mentions").unlink()
        # Exit 0 without ok:true from --launcher-verify is a refusal.
        self.flag("launcher-says-not-ok")
        self.assertIn("did not report ok", self.check().stdout)
        (self.tools / "launcher-says-not-ok").unlink()
        # A granted answer from a launcher not started by launchd is not the launcher's own.
        result = self.check("--activate", FAKE_FDA="granted", FAKE_ERRNO="0", FAKE_PPID="4242")
        self.assertEqual(result.returncode, 1)
        self.assertIn("not launchd", result.stdout)
        self.assertFalse(self.record.exists())
        # A malformed team is refused.
        self.assertEqual(self.check("--verify-bundle", str(self.app), FAKE_TEAM="not set").returncode, 1)

    def test_a_hung_tool_fails(self):
        self.install()
        (self.tools / "codesign").write_text("#!/bin/bash\nsleep 30\n")
        result = self.check("--verify-bundle", str(self.app), SHIPYARD_LAUNCHER_CHECK_TOOL_TIMEOUT_SECS="1")
        self.assertEqual(result.returncode, 1)

    def test_refused_release_is_not_ready(self):
        self.install()
        self.flag("launcher-refuses")
        result = self.check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("FAIL release", result.stdout)

    def test_without_full_disk_access_names_the_owner_step_and_will_not_activate(self):
        self.install()
        result = self.check("--activate")
        self.assertEqual(result.returncode, 1)
        self.assertIn("FAIL full_disk_access: denied", result.stdout)
        self.assertIn("Privacy & Security > Full Disk Access", result.stdout)
        self.assertFalse(self.record.exists())
        args = (self.tools / "open-args").read_text()
        self.assertIn(f"-n -g -a {self.app} --args --launcher-check-access --launcher-result", args)

    def test_granted_then_activate_and_deactivate(self):
        self.install()
        result = self.check(FAKE_FDA="granted", FAKE_ERRNO="0")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("Ready: run with --activate", result.stdout)
        self.assertFalse(self.record.exists(), "a plain check is read-only")
        result = self.check("--activate", FAKE_FDA="granted", FAKE_ERRNO="0")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        record = json.loads(self.record.read_text())
        self.assertEqual(record["schema"], 2)
        self.assertEqual(record["team"], "TEAM123456")
        self.assertEqual(record["identifier"], "com.danielraffel.shipyard.launcher")
        self.assertEqual(record["cdhash"], "0123456789abcdef0123456789abcdef01234567")
        self.assertEqual(record["app"], os.path.realpath(self.app))
        self.assertIn('certificate leaf[subject.OU] = "TEAM123456"', record["designated_requirement"])
        self.assertEqual(stat.S_IMODE(self.record.stat().st_mode), 0o600)
        self.assertIn("activated: yes", result.stdout)
        result = self.check("--deactivate")
        self.assertEqual(result.returncode, 0)
        self.assertFalse(self.record.exists())

    def test_activation_the_launcher_rejects_is_removed(self):
        self.install()
        self.flag("launcher-inactive")
        result = self.check("--activate", FAKE_FDA="granted", FAKE_ERRNO="0")
        self.assertEqual(result.returncode, 1)
        self.assertIn("did not accept the new record", result.stderr)
        self.assertFalse(self.record.exists())

    def test_activation_needs_the_bundle_team_and_a_cdhash(self):
        self.install()
        result = self.check("--activate", FAKE_FDA="granted", FAKE_ERRNO="0", FAKE_BUNDLE_TEAM="OTHERTEAM1")
        self.assertEqual(result.returncode, 1)
        self.assertFalse(self.record.exists())
        result = self.check("--activate", FAKE_FDA="granted", FAKE_ERRNO="0", FAKE_CDHASH="not-a-hash")
        self.assertEqual(result.returncode, 1)
        self.assertFalse(self.record.exists())

    def test_status_reports_the_launchers_verdict_not_the_file(self):
        self.install()
        self.record.parent.mkdir(parents=True)
        self.record.write_text('{"schema": 2}')
        self.flag("launcher-inactive")
        report = json.loads(self.check("--json").stdout)
        self.assertFalse(report["activated"])
        self.assertTrue(report["activation"].startswith("bundle_replaced"))

    def test_launch_services_unavailable_is_unknown(self):
        self.install()
        self.flag("open-fails")
        result = self.check("--json")
        self.assertEqual(result.returncode, 1)
        report = json.loads(result.stdout)
        self.assertEqual(report["checks"]["full_disk_access"]["state"], "unknown")
        self.assertIn("GUI session", self.check().stdout)


if __name__ == "__main__":
    unittest.main()

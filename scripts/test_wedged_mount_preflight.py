from __future__ import annotations

import contextlib
import importlib.util
import io
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

SCRIPT = Path(__file__).with_name("wedged_mount_preflight.py")
SPEC = importlib.util.spec_from_file_location("wedged_mount_preflight", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
preflight = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(preflight)

# The probe, except that stat-ing a path ending in "wedged" blocks the way a
# stuck DeviceFS mount did on m3 (2026-10-20).
WEDGING_PROBE = preflight.PROBE.replace(
    "        os.stat(path)",
    "        os.stat(path)\n        if path.endswith('wedged'):\n            import time; time.sleep(60)",
)
# m3's process table on 2026-10-20, trimmed.
M3_PS = """\
41012 Us   02-06:52:28 /Library/Developer/PrivateFrameworks/CoreDevice.framework/DeviceFS.appex/Contents/MacOS/DeviceFS
30577 U       01:07:45 /usr/sbin/lsof -nP -S 2 -F pf -- /Users/x/Library/Application Support/shipyard/.sandbox-writer-domain.lock
 5150 U          00:03 /usr/bin/rsync -a src dst
 7001 S          10:00 /bin/zsh
"""


class WedgedMountPreflight(unittest.TestCase):
    def mount_table(self, directory: Path, *names: str) -> Path:
        lines = []
        for name in names:
            point = directory / name
            point.mkdir()
            kind = "devicefs" if name.endswith("wedged") else "apfs"
            lines.append(f"dev-{name} on {point} ({kind}, local, nobrowse)")
        table = directory / "mount.txt"
        table.write_text("\n".join(lines) + "\n", encoding="utf-8")
        return table

    def run_main(self, *args: str) -> tuple[int, str]:
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
            code = preflight.main(list(args))
        return code, out.getvalue()

    def test_mount_points_keep_spaces_in_paths(self) -> None:
        table = ("/dev/disk3s1s1 on / (apfs, sealed, local)\n"
                 "devices -- file:///x/ on /Users/x/Library/Developer/CoreDevice/DeviceFS "
                 "(devicefs, local, nodev)\n"
                 "//u@h/share on /Volumes/My Share (smbfs, nodev)\n")
        self.assertEqual(
            [path for path, _ in preflight.mount_points(table)],
            ["/", "/Users/x/Library/Developer/CoreDevice/DeviceFS", "/Volumes/My Share"],
        )

    def test_every_mount_answering_passes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            table = self.mount_table(Path(directory), "a", "b")
            code, out = self.run_main("--mount-output", str(table))
        self.assertEqual(code, 0, out)
        self.assertIn("2 mount points answered", out)

    def test_a_mount_whose_stat_blocks_fails_and_is_named(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            table = self.mount_table(Path(directory), "a", "devicefs-wedged", "c")
            ps_file = Path(directory) / "ps.txt"
            ps_file.write_text(M3_PS, encoding="utf-8")
            started = time.monotonic()
            with mock.patch.object(preflight, "PROBE", WEDGING_PROBE):
                code, out = self.run_main("--mount-output", str(table), "--ps-output",
                                          str(ps_file), "--timeout", "1")
            self.assertLess(time.monotonic() - started, 10)
        self.assertEqual(code, 1, out)
        self.assertIn("::error title=wedged mount::stat of mount point", out)
        self.assertIn("devicefs-wedged did not return in 1s", out)
        self.assertIn("(devicefs, local", out)
        self.assertIn("pid=41012", out)
        self.assertIn("pid=30577", out)
        self.assertNotIn("pid=5150", out, "a brief wait is not listed")

    def test_an_empty_mount_table_is_unreadable_not_healthy(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            table = Path(directory) / "mount.txt"
            table.write_text("", encoding="utf-8")
            self.assertEqual(self.run_main("--mount-output", str(table))[0], 2)

    def test_elapsed_time_formats(self) -> None:
        self.assertEqual(preflight.elapsed_seconds("00:03"), 3)
        self.assertEqual(preflight.elapsed_seconds("01:07:45"), 4065)
        self.assertEqual(preflight.elapsed_seconds("02-06:52:28"), 2 * 86400 + 24748)


if __name__ == "__main__":
    unittest.main()

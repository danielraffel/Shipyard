from __future__ import annotations

import contextlib
import importlib.util
import io
import os
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

HOOK = "def hook(call, path):\n    pass\n"


def probe_blocking_in(*calls: str) -> str:
    """The probe, except that ``calls`` on a path ending in "wedged" block the
    way the stuck DeviceFS mount did on m3 (2026-10-20). There the root's stat
    returned and the directory read did not."""
    assert preflight.PROBE.count(HOOK) == 1
    return preflight.PROBE.replace(
        HOOK,
        "def hook(call, path):\n"
        f"    if path.endswith('wedged') and call in {calls!r}:\n"
        "        import time; time.sleep(60)\n",
    )


WEDGING_PROBE = probe_blocking_in("stat")
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
        self.assertIn("returned for 2 mount points", out)
        self.assertIn("does not prove", out, "a pass must not read as a healthy host")

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
        self.assertRegex(out, r"::error title=wedged mount::stat of mount point \S+/devicefs-wedged ")
        self.assertIn("(devicefs, local", out)
        self.assertIn("pid=41012", out)
        self.assertIn("pid=30577", out)
        self.assertNotIn("pid=5150", out, "a brief wait is not listed")

    def test_a_mount_whose_stat_answers_but_whose_server_is_stuck_fails(self) -> None:
        # m3, 2026-10-20: the root's stat answered from the kernel while the
        # server behind it was stuck. A stat-only probe passed that mount.
        for call in ("statvfs", "readdir"):
            with self.subTest(call=call), tempfile.TemporaryDirectory() as directory:
                table = self.mount_table(Path(directory), "a", "devicefs-wedged", "c")
                started = time.monotonic()
                with mock.patch.object(preflight, "PROBE", probe_blocking_in(call)):
                    code, out = self.run_main("--mount-output", str(table), "--timeout", "1",
                                              "--ps-output", os.devnull)
                self.assertLess(time.monotonic() - started, 10)
                self.assertEqual(code, 1, out)
                self.assertRegex(out, rf"::error title=wedged mount::{call} of mount point "
                                      r"\S+/devicefs-wedged did not return in 1s")

    def test_the_probe_reaches_the_filesystem_on_every_mount(self) -> None:
        # Negative control for the test above: the calls the probe makes are
        # exactly stat, statvfs and a directory read, in that order, per mount.
        with tempfile.TemporaryDirectory() as directory:
            points = [str(Path(directory) / name) for name in ("a", "b")]
            for point in points:
                Path(point).mkdir()
                (Path(point) / "entry").write_text("x", encoding="utf-8")
            output, finished = preflight.probe_mounts(points, 10)
        self.assertTrue(finished, output)
        calls = [line.split(" ", 1) for line in output.splitlines()
                 if line not in ("ok", "done")]
        self.assertEqual(calls, [[call, point] for point in points
                                 for call in ("stat", "statvfs", "readdir")])

    def test_the_directory_read_reads_an_entry(self) -> None:
        # Opening a directory need not reach the server; reading from it does.
        namespace: dict = {}
        with mock.patch("sys.argv", ["probe"]), contextlib.redirect_stdout(io.StringIO()):
            exec(preflight.PROBE, namespace)
        read = []

        class Entries:
            def __enter__(self) -> "Entries":
                return self

            def __exit__(self, *_: object) -> None:
                return None

            def __iter__(self) -> "Entries":
                return self

            def __next__(self) -> str:
                read.append(True)
                raise StopIteration

        with mock.patch.object(os, "scandir", return_value=Entries()) as scandir:
            namespace["readdir"]("/mnt")
        scandir.assert_called_once_with("/mnt")
        self.assertEqual(read, [True])

    def test_where_the_probe_stopped_names_mount_and_call(self) -> None:
        self.assertIsNone(preflight.wedged_mount("stat /a\nok\ndone\n", True))
        self.assertEqual(preflight.wedged_mount("stat /a\nok\nstatvfs /a\n", False),
                         ("/a", "statvfs"))
        self.assertEqual(preflight.wedged_mount("stat /a b\nok\nreaddir /a b\n", False),
                         ("/a b", "readdir"))
        self.assertEqual(preflight.wedged_mount("", False), ("(before the first mount)", "start"))

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

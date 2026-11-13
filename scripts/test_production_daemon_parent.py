#!/usr/bin/env python3
from __future__ import annotations

import os
import unittest

import production_daemon_parent as pdp


LAUNCHER = "/Users/u/.local/libexec/shipyard/shipyard-daemon-launcher"
INSTALLED = "/Users/u/.local/bin/shipyard"
DAEMON = f"{INSTALLED} --mode shipyard daemon run --repo a/b"
LAUNCHER_CMD = f"{LAUNCHER} --mode shipyard daemon supervise --exec {INSTALLED} --repo a/b --repo c/d"


def reader_for(table):
    return lambda pid: table.get(pid)


def classify(table, pid=100):
    return pdp.classify_parent(
        pid, launcher=LAUNCHER, installed=INSTALLED, reader=reader_for(table)
    )


class ClassifyParentTests(unittest.TestCase):
    def test_launchd_parent_ok(self):
        ok, _ = classify({100: pdp.ProcessInfo(1, DAEMON)})
        self.assertTrue(ok)

    def test_launcher_under_launchd_ok(self):
        ok, why = classify({
            100: pdp.ProcessInfo(50, DAEMON),
            50: pdp.ProcessInfo(1, LAUNCHER_CMD),
        })
        self.assertTrue(ok, why)

    def test_launcher_without_repos_ok(self):
        ok, why = classify({
            100: pdp.ProcessInfo(50, DAEMON),
            50: pdp.ProcessInfo(1, f"{LAUNCHER} --mode shipyard daemon supervise --exec {INSTALLED}"),
        })
        self.assertTrue(ok, why)

    def test_other_parent_fails(self):
        for command in (
            "/bin/sh /tmp/canary/sandbox-daemon-guardian.py",
            f"/tmp/candidate/shipyard --mode shipyard daemon supervise --exec {INSTALLED}",
            f"/tmp/x/{LAUNCHER} --mode shipyard daemon supervise --exec {INSTALLED}",
            f"{LAUNCHER} --mode shipyard daemon supervise --exec /tmp/candidate/shipyard",
            f"{LAUNCHER} --mode shipyard daemon supervise --in-place --exec {INSTALLED}",
            f"{LAUNCHER} --mode sandbox daemon supervise --exec {INSTALLED}",
            f"{LAUNCHER} --mode shipyard daemon supervise --exec {INSTALLED}x",
            f"{LAUNCHER} --mode shipyard daemon supervise --exec {INSTALLED} --state-dir /tmp/s",
        ):
            with self.subTest(command=command):
                ok, _ = classify({
                    100: pdp.ProcessInfo(50, DAEMON),
                    50: pdp.ProcessInfo(1, command),
                })
                self.assertFalse(ok)

    def test_launcher_not_under_launchd_fails(self):
        ok, why = classify({
            100: pdp.ProcessInfo(50, DAEMON),
            50: pdp.ProcessInfo(40, LAUNCHER_CMD),
            40: pdp.ProcessInfo(1, "/bin/bash"),
        })
        self.assertFalse(ok)
        self.assertIn("not launchd", why)

    def test_missing_daemon_or_parent_fails(self):
        self.assertFalse(classify({})[0])
        self.assertFalse(classify({100: pdp.ProcessInfo(50, DAEMON)})[0])

    def test_wait_retries_until_supervised(self):
        states = iter([
            {100: pdp.ProcessInfo(77, DAEMON), 77: pdp.ProcessInfo(1, "/bin/sh x")},
            {100: pdp.ProcessInfo(1, DAEMON)},
        ])
        current = {}
        now = [0.0]

        def reader(pid):
            return current.get(pid)

        def sleep(seconds):
            now[0] += seconds
            current.clear()
            current.update(next(states))

        current.update(next(states))
        ok, _ = pdp.wait_for_supervised_parent(
            100, launcher=LAUNCHER, installed=INSTALLED, wait_seconds=10,
            reader=reader, clock=lambda: now[0], sleep=sleep,
        )
        self.assertTrue(ok)

    def test_wait_gives_up_after_deadline(self):
        now = [0.0]
        table = {100: pdp.ProcessInfo(77, DAEMON), 77: pdp.ProcessInfo(1, "/bin/sh x")}

        def sleep(seconds):
            now[0] += seconds

        ok, _ = pdp.wait_for_supervised_parent(
            100, launcher=LAUNCHER, installed=INSTALLED, wait_seconds=3,
            reader=reader_for(table), clock=lambda: now[0], sleep=sleep,
        )
        self.assertFalse(ok)
        self.assertGreaterEqual(now[0], 3)

    def test_read_process_live(self):
        info = pdp.read_process(os.getpid())
        self.assertIsNotNone(info)
        self.assertEqual(info.ppid, os.getppid())
        self.assertIn("python", info.command)


if __name__ == "__main__":
    unittest.main()

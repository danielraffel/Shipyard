"""Fake transports exercise orchestration, not Rust's receipt validation."""
import argparse
import asyncio
import contextlib
import datetime as dt
import fcntl
import gc
import hashlib
import io
import json
import os
from pathlib import Path
import shlex
import sys
import tempfile
import time
import unittest
import warnings
from unittest import mock

import collect_fleet_capacity as collector


def host(name="alpha"):
    return {"ssh_alias": name, "host_id": name, "tartci": "/opt/tools with spaces/tartci",
            "repo": "example/project", "target": "macos"}


def sample(name="alpha", verification="verified"):
    observed = collector.now()
    # These are envelope fixtures only. The fake importer does not claim to
    # validate their placeholder digests or evidence; real integration is separate.
    return {"schema": collector.SCHEMA, "sample_id": "a" * 64,
            "observed_at": collector.stamp(observed),
            "valid_until": collector.stamp(observed + dt.timedelta(seconds=120)),
            "host_id": name, "host_boot_id": "boot-a", "generation": "f" * 40,
            "repo": "example/project", "target": "macos", "profile_sha256": "b" * 64,
            "configured_slots": 1, "eligible_slots": 1 if verification == "verified" else None,
            "free_slots": 1 if verification == "verified" else None,
            "occupied_slots": 0 if verification == "verified" else None,
            "in_flight_slots": 0 if verification == "verified" else None,
            "unknown_slots": 0 if verification == "verified" else 1,
            "pool_state": "on", "source": "tartci", "auth_method": "untrusted_local",
            "verification": verification, "evidence": {}, "evidence_sha256": "c" * 64,
            "census_sha256": "d" * 64}


class CapacityEnvelopeTests(unittest.TestCase):
    def test_verified_and_complete_unknown_envelopes_reach_importer_boundary(self):
        for verification, code in (("verified", 0), ("unknown", 2)):
            value = sample(verification=verification)
            self.assertEqual(collector.receipt_envelope(collector.encode(value), code, host(), collector.now(), 5), value)

    def test_noise_usage_identity_clock_and_transport_failures_are_rejected(self):
        now = collector.now()
        base = sample()
        cases = [(b"usage: tartci", 2), (b"notice\n" + collector.encode(base), 0),
                 (collector.encode({"schema": collector.SCHEMA, "verification": "unknown"}), 2),
                 (collector.encode(base), 255), (collector.encode(base), 2),
                 (collector.encode(dict(base, host_id="wrong")), 0),
                 (collector.encode(dict(base, repo="other/project")), 0),
                 (collector.encode(dict(base, target="linux")), 0),
                 (collector.encode(dict(base, auth_method="ssh")), 0),
                 (collector.encode(dict(base, valid_until=collector.stamp(now - dt.timedelta(seconds=1)))), 0),
                 (collector.encode(dict(base, observed_at=collector.stamp(now + dt.timedelta(seconds=30)))), 0),
                 (collector.encode(dict(base, observed_at="2026-01-01T00:00:00")), 0)]
        for payload, code in cases:
            with self.subTest(payload=payload, code=code), self.assertRaises(collector.ObservationError):
                collector.receipt_envelope(payload, code, host(), now, 5)

    def test_remote_arguments_are_quoted_without_host_override(self):
        argv = collector.ssh_command("ssh", host())
        remote = shlex.split(argv[-1])
        self.assertEqual(remote[0], host()["tartci"])
        self.assertEqual(argv[-2], "alpha")
        self.assertNotIn("--host-id", remote)
        self.assertIn("StrictHostKeyChecking=yes", argv)
        self.assertIn("ControlPath=none", argv)
        self.assertEqual(remote[-3:], ["--valid-for-seconds", "120", "--json"])

    def test_duplicate_json_keys_are_rejected(self):
        with self.assertRaises(collector.ObservationError):
            collector.strict_json(b'{"a":1,"a":2}')


class CapacityFixture:
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.output = self.root / "observer"
        self.roster = self.root / "roster.json"
        self.ssh = self.root / "fake-ssh"
        self.cli = self.root / "fake-shipyard"
        self.env = mock.patch.dict(os.environ, {"CAPACITY_TEST_ROOT": str(self.root), "SHIPYARD_OBSERVATIONS": "on"})
        self.env.start()
        self.addCleanup(self.env.stop)
        self.ssh.write_text(f"#!{sys.executable}\n" + '''
import json, os, pathlib, sys, time
root = pathlib.Path(os.environ["CAPACITY_TEST_ROOT"])
alias = sys.argv[-2]
config = json.loads((root / "responses.json").read_text())[alias]
(root / (alias + ".argv")).write_text(json.dumps(sys.argv[1:]))
mode = config.get("mode", "receipt")
if mode == "hang":
    time.sleep(10)
if mode == "oversize":
    sys.stdout.write("x" * 2000000); sys.stdout.flush(); time.sleep(10)
if mode == "usage":
    print("usage: tartci fleet capacity"); sys.exit(2)
if mode == "transport":
    sys.exit(255)
if mode == "noise":
    print("remote shell banner")
print(json.dumps(config["receipt"]))
sys.exit(2 if config["receipt"]["verification"] == "unknown" else 0)
''')
        self.cli.write_text(f"#!{sys.executable}\n" + '''
import json, os, pathlib, sys, time
root = pathlib.Path(os.environ["CAPACITY_TEST_ROOT"])
payload = sys.stdin.read()
with (root / "imports.jsonl").open("a") as log:
    log.write(json.dumps({"argv": sys.argv[1:], "payload": payload}) + "\\n")
mode = (root / "import-mode").read_text() if (root / "import-mode").exists() else "accept"
if mode == "hang":
    time.sleep(10)
if mode == "reject":
    print("invalid digest", file=sys.stderr); sys.exit(1)
if mode == "malformed":
    print("not an acknowledgement"); sys.exit(0)
# Mirrors MetricsImportOutput and write_output in src/app/metrics_cmd.rs.
print(json.dumps({"database": str(root / "state/metrics/metrics.db"),
                  "source": "fleet-capacity", "imported": len(json.loads(payload))}))
''')
        self.ssh.chmod(0o755)
        self.cli.chmod(0o755)

    def configure(self, responses):
        self.roster.write_text(json.dumps({"schema": collector.ROSTER_SCHEMA, "hosts": [host(name) for name in responses]}))
        (self.root / "responses.json").write_text(json.dumps(responses))

    def run_pass(self, extra=()):
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(io.StringIO()):
            code = collector.main(["--roster", str(self.roster), "--state-dir", str(self.root / "state"),
                                   "--output-dir", str(self.output), "--ssh", str(self.ssh),
                                   "--shipyard", str(self.cli), "--deadline-seconds", "3",
                                   "--fetch-seconds", "0.7", *extra])
        return code, json.loads(output.getvalue()) if output.getvalue() else None

    def imports(self):
        path = self.root / "imports.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


class CapacityPullerTests(CapacityFixture, unittest.TestCase):
    def test_one_batch_keeps_unknown_and_omits_missing_hosts(self):
        self.configure({"alpha": {"receipt": sample()},
                        "beta": {"receipt": sample("beta", "unknown")},
                        "gamma": {"mode": "usage"}, "delta": {"mode": "transport"}})
        code, status = self.run_pass()
        self.assertEqual(code, 2)
        self.assertEqual(len(self.imports()), 1)
        batch = json.loads(self.imports()[0]["payload"])
        self.assertEqual([value["host_id"] for value in batch], ["alpha", "beta"])
        self.assertEqual(batch[1]["verification"], "unknown")
        self.assertEqual([row["status"] for row in status["hosts"]], ["received", "received", "missing", "missing"])
        self.assertEqual(self.imports()[0]["argv"][-4:], ["metrics", "import", "fleet-capacity", "--json"])
        self.assertEqual(json.loads((self.output / "last-status.json").read_text()), status)
        self.assertEqual(len((self.output / "attempts.jsonl").read_text().splitlines()), 1)

    def test_verified_control_completes_and_records_roster_and_source(self):
        self.configure({"alpha": {"receipt": sample()}})
        code, status = self.run_pass()
        self.assertEqual(code, 0)
        self.assertEqual(status["status"], "complete")
        self.assertEqual(len(status["roster_sha256"]), 64)
        self.assertEqual(len(status["source_sha256"]), 64)
        self.assertEqual(status["importer"]["path"], str(self.cli.resolve()))
        self.assertEqual(status["importer"]["sha256"], hashlib.sha256(self.cli.read_bytes()).hexdigest())
        self.assertNotIn("evidence", status["hosts"][0])

    def test_off_returns_disabled_before_roster_process_or_storage_work(self):
        with mock.patch.dict(os.environ, {"SHIPYARD_OBSERVATIONS": "off"}), \
                mock.patch.object(collector, "read_roster") as roster, \
                mock.patch.object(collector, "importer_identity") as importer:
            code, status = self.run_pass()
        self.assertEqual(code, 0)
        self.assertEqual(status["status"], "disabled")
        self.assertFalse(status["attempted"])
        self.assertNotIn("pass_id", status)
        roster.assert_not_called()
        importer.assert_not_called()
        self.assertFalse(self.output.exists())
        self.assertEqual(self.imports(), [])

    def test_four_host_default_budget_supports_five_day_fixture_window(self):
        self.configure({name: {"receipt": sample(name)} for name in ("alpha", "beta", "gamma", "delta")})
        code, status = self.run_pass()
        self.assertEqual(code, 0)
        pass_bytes = len(collector.encode(status))
        five_day_bytes = pass_bytes * 5 * 24 * 60
        print(f"capacity four-host fixture: bytes_per_pass={pass_bytes} five_day_bytes={five_day_bytes} "
              f"default_history_bytes={collector.DEFAULT_HISTORY_BYTES}")
        self.assertLess(five_day_bytes, collector.DEFAULT_HISTORY_BYTES)
        self.assertEqual(status["history_retention"]["retained_passes"], 1)
        # A one-pass retained window is not five days of reliability evidence.
        self.assertEqual(status["history_retention"]["oldest_retained_at"], status["completed_at"])

    def test_all_missing_and_bounded_parallel_hangs_do_not_import(self):
        self.configure({"alpha": {"mode": "hang"}, "beta": {"mode": "hang"},
                        "gamma": {"mode": "oversize"}, "delta": {"mode": "usage"}})
        started = time.monotonic()
        code, status = self.run_pass()
        self.assertLess(time.monotonic() - started, 3)
        self.assertEqual(code, 2)
        self.assertEqual(len(status["hosts"]), 4)
        self.assertTrue(all(row["status"] == "missing" for row in status["hosts"]))
        self.assertIn("output_byte_limit", [row["reason"] for row in status["hosts"]])
        self.assertEqual(self.imports(), [])

    def test_import_failure_is_unconfirmed_without_retry(self):
        for mode in ("reject", "malformed", "hang"):
            with self.subTest(mode=mode):
                self.configure({"alpha": {"receipt": sample()}})
                (self.root / "import-mode").write_text(mode)
                before = len(self.imports())
                started = time.monotonic()
                code, status = self.run_pass()
                self.assertLess(time.monotonic() - started, 4)
                self.assertEqual(code, 2)
                self.assertEqual(status["hosts"][0]["import"], "unconfirmed")
                self.assertEqual(len(self.imports()), before + 1)

    def test_overlap_is_journaled_without_ssh_or_import(self):
        self.configure({"alpha": {"receipt": sample()}})
        self.output.mkdir()
        with (self.output / "pass.lock").open("a+b") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            code, status = self.run_pass()
        self.assertEqual(code, 2)
        self.assertEqual(status["status"], "skipped_overlap")
        self.assertEqual(status["hosts"][0]["reason"], "skipped_overlap")
        self.assertFalse((self.root / "alpha.argv").exists())
        self.assertEqual(self.imports(), [])
        self.assertEqual(len((self.output / "attempts.jsonl").read_text().splitlines()), 1)

    def test_invalid_roster_is_rejected_before_subprocesses(self):
        self.configure({"alpha": {"receipt": sample()}})
        bad_hosts = [dict(host(), ssh_alias="-oProxyCommand=bad"), dict(host(), tartci="relative/tartci"),
                     dict(host(), tartci="/opt/../tartci"), dict(host(), repo="invalid")]
        for bad in bad_hosts:
            with self.subTest(host=bad):
                self.roster.write_text(json.dumps({"schema": collector.ROSTER_SCHEMA, "hosts": [bad]}))
                code, status = self.run_pass()
                self.assertEqual(code, 1)
                self.assertIsNone(status)
        self.assertFalse((self.root / "alpha.argv").exists())
        self.assertEqual(self.imports(), [])


class CapacityJournalTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.args = argparse.Namespace(output_dir=self.root, history_days=7, history_bytes=2048)

    def record(self, index=0, age=0, generation="f" * 40):
        return {"schema": collector.STATUS_SCHEMA, "pass_id": str(index), "roster_sha256": "a" * 64,
                "completed_at": collector.stamp(collector.now() - dt.timedelta(days=age)),
                "hosts": [{"host_id": "alpha", "host_boot_id": "boot", "generation": generation,
                           "import": "accepted"}]}

    def test_age_and_byte_rotation_preserve_recent_missingness(self):
        (self.root / "attempts.jsonl").write_bytes(collector.encode(self.record(age=8)))
        for index in range(20):
            record = self.record(index)
            if index == 19:
                record["hosts"] = [{"host_id": "alpha", "status": "missing", "reason": "deadline", "import": "not_submitted"}]
            collector.persist(self.args, record)
            self.assertLessEqual((self.root / "attempts.jsonl").stat().st_size, 2048)
        rows = [json.loads(line) for line in (self.root / "attempts.jsonl").read_text().splitlines()]
        self.assertLess(len(rows), 20)
        self.assertEqual(rows[-1]["hosts"][0]["status"], "missing")
        self.assertEqual(rows[-1]["pass_id"], "19")
        self.assertEqual(rows[-1]["history_retention"]["retained_passes"], len(rows))
        self.assertEqual(rows[-1]["history_retention"]["oldest_retained_at"], rows[0]["completed_at"])
        self.assertTrue(any(row["history_retention"]["byte_dropped_this_pass"] > 0 for row in rows))

    def test_generation_changes_use_equality_not_order(self):
        collector.persist(self.args, self.record())
        current = self.record(1, generation="0" * 40)
        collector.persist(self.args, current)
        self.assertTrue(current["hosts"][0]["identity_changed"])
        same = self.record(2, generation="0" * 40)
        collector.persist(self.args, same)
        self.assertFalse(same["hosts"][0]["identity_changed"])

    def test_failed_atomic_replace_preserves_status_and_removes_temporary(self):
        path = self.root / "last-status.json"
        path.write_bytes(b"previous")
        with mock.patch.object(collector.os, "replace", side_effect=OSError("write failed")):
            with self.assertRaises(OSError):
                collector.atomic_write(path, b"new")
        self.assertEqual(path.read_bytes(), b"previous")
        self.assertEqual(list(self.root.iterdir()), [path])


class CapacityProcessTests(unittest.IsolatedAsyncioTestCase):
    async def test_oversize_pipes_keep_primary_reason_and_close_transports(self):
        loop = asyncio.get_running_loop()
        exceptions, protocols = [], []
        previous_handler = loop.get_exception_handler()
        original_protocol = collector.CaptureProtocol

        def capture(*args):
            protocol = original_protocol(*args)
            protocols.append(protocol)
            return protocol

        loop.set_exception_handler(lambda _loop, context: exceptions.append(context))
        try:
            # Flush warnings left by earlier helper tests before narrowing the
            # capture to resources owned by this subprocess exercise.  A
            # collection after the commands would also collect unrelated
            # stale SQLite fixtures from the suite and make this test fail on
            # Python 3.14 for the wrong reason.
            gc.collect()
            with warnings.catch_warnings(record=True) as caught:
                warnings.simplefilter("always", ResourceWarning)
                with mock.patch.object(collector, "CaptureProtocol", side_effect=capture):
                    for fd in (1, 2):
                        for linger in (0, 10):
                            with self.subTest(fd=fd, linger=linger):
                                code = f"import os,time; os.write({fd}, b'x' * 2000000); time.sleep({linger})"
                                with self.assertRaisesRegex(collector.ObservationError, "^output_byte_limit$"):
                                    await collector.command([sys.executable, "-c", code], time.monotonic() + 3, 1024)
                                protocol = protocols[-1]
                                self.assertTrue(protocol.closed.done())
                                self.assertTrue(protocol.transport.is_closing())
                                self.assertLessEqual(len(protocol.stdout), 1024)
                protocols.clear()
                await asyncio.sleep(0)
            self.assertFalse([item for item in caught if issubclass(item.category, ResourceWarning)], caught)
            self.assertEqual(exceptions, [])
        finally:
            loop.set_exception_handler(previous_handler)

    async def test_timeout_kills_only_its_owned_process_group(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / "orphan-marker"
            code = "import os,time,pathlib,sys; child=os.fork(); time.sleep(0.3 if child==0 else 10); pathlib.Path(sys.argv[1]).write_text('orphan')"
            with self.assertRaisesRegex(collector.ObservationError, "deadline"):
                await collector.command([sys.executable, "-c", code, str(marker)], time.monotonic() + 0.1, 1024)
            await asyncio.sleep(0.4)
            self.assertFalse(marker.exists())


@unittest.skipUnless(os.environ.get("SHIPYARD_CAPACITY_TEST_CLI"), "requires owner-admitted exact Shipyard binary")
class CapacityRealImporterTests(CapacityFixture, unittest.TestCase):
    """Optional real Rust contract check; SSH remains fake and state is temporary."""
    def test_real_importer_accepts_receipts_and_rejects_tampering(self):
        self.cli = Path(os.environ["SHIPYARD_CAPACITY_TEST_CLI"])

        def receipt_fixture(verification):
            value = sample(verification=verification)
            observed = collector.now().replace(microsecond=0)
            value["observed_at"] = collector.stamp(observed).replace("+00:00", "Z")
            value["valid_until"] = collector.stamp(observed + dt.timedelta(seconds=120)).replace("+00:00", "Z")
            value["evidence"] = {
                "schema": "tartci.gate-supply/v1", "host": "alpha", "repo": "example/project",
                "class": "macos", "profile_sha256": value["profile_sha256"],
                "verdict": "ok" if verification == "verified" else "unknown", "pool": "on",
                "free": 1 if verification == "verified" else 0, "occupied": 0, "in_flight": 0,
                "lanes": [{"lane": "a"}],
            }
            digest = lambda item: hashlib.sha256(collector.encode(item)).hexdigest()
            value["evidence_sha256"] = digest(value["evidence"])
            value["census_sha256"] = digest({key: value["evidence"][key] for key in
                                              ("schema", "host", "repo", "class", "verdict", "pool", "lanes")})
            value["sample_id"] = digest({key: item for key, item in value.items() if key != "sample_id"})
            return value

        for verification in ("verified", "unknown"):
            self.configure({"alpha": {"receipt": receipt_fixture(verification)}})
            code, status = self.run_pass()
            self.assertEqual(code, 0 if verification == "verified" else 2)
            self.assertEqual(status["hosts"][0]["import"], "accepted", status)
        bad = receipt_fixture("verified")
        bad["sample_id"] = "e" * 64
        self.configure({"alpha": {"receipt": bad}})
        code, status = self.run_pass()
        self.assertEqual(code, 2)
        self.assertEqual(status["hosts"][0]["import"], "unconfirmed")


if __name__ == "__main__":
    unittest.main()

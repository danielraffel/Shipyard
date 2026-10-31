#!/usr/bin/env python3
from __future__ import annotations

import datetime as dt
import hashlib
import importlib.util
import json
import os
import pathlib
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "merge_receipt_handoff.py"
SPEC = importlib.util.spec_from_file_location("merge_receipt_handoff", SCRIPT)
MOD = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MOD)
handoff = MOD


def receipt(repo: str = "owner/repo") -> dict[str, object]:
    return {
        "schema": "shipyard-merge-receipt/v1",
        "receipt_id": "r-1",
        "repo": repo,
        "project": repo.split("/")[-1],
        "base_branch": "main",
        "pr": 1,
        "head_sha": "a" * 40,
        "validated_head_sha": "a" * 40,
        "merge_commit_sha": "b" * 40,
        "merged_at": "2026-10-26T00:00:00Z",
        "safe_merge": True,
        "source": "shipyard",
        "required_checks": ["test"],
        "gate_receipt_sha256": "c" * 64,
        "evidence": "sha256:r-1",
    }


class MergeReceiptHandoffTests(unittest.TestCase):
    @unittest.skipUnless(hasattr(os, "O_DIRECTORY"), "directory fsync is unavailable")
    def test_atomic_write_syncs_parent_directory_after_replace(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            path = root / "handoff.jsonl"
            real_open = handoff.os.open
            real_close = handoff.os.close
            real_fsync = handoff.os.fsync
            opened: list[tuple[object, int]] = []
            synced: list[int] = []

            def open_wrapper(name: object, flags: int, *args: object) -> int:
                fd = real_open(name, flags, *args)
                opened.append((name, fd))
                return fd

            with mock.patch.object(handoff.os, "open", side_effect=open_wrapper), \
                 mock.patch.object(handoff.os, "close", side_effect=real_close), \
                 mock.patch.object(handoff.os, "fsync", side_effect=lambda fd: (synced.append(fd), real_fsync(fd))[1]):
                handoff.atomic_write(path, "payload\n")

            parent_fds = {fd for name, fd in opened if pathlib.Path(name) == root}
            self.assertTrue(parent_fds & set(synced))
            self.assertEqual(path.read_text(), "payload\n")

    def health(self, handoff: pathlib.Path, *, now: dt.datetime | None = None, **overrides: object) -> dict[str, object]:
        observed = now or dt.datetime.now(dt.timezone.utc)
        payload = handoff.read_bytes()
        value: dict[str, object] = {
            "schema": "shipyard-merge-receipt-handoff/v1", "repo": "owner/repo", "project": "repo",
            "base_branch": "main", "file": str(handoff), "status": "fresh",
            "freshness_basis": "handoff_export_and_source_import", "checked_at": observed.isoformat().replace("+00:00", "Z"),
            "exported_at": observed.isoformat().replace("+00:00", "Z"),
            "latest_merged_at": "2026-10-26T00:00:00Z", "source_imported_at": observed.isoformat().replace("+00:00", "Z"),
            "stale_after_seconds": 7200,
            "receipt_count": 1,
            "handoff_bytes": len(payload), "handoff_sha256": hashlib.sha256(payload).hexdigest(),
            "producer": {"identity": "shipyard", "version": "shipyard 0.366.4", "executable_sha256": "a" * 64},
        }
        value.update(overrides)
        return value

    def producer_db(self, root: pathlib.Path, imported_at: str = "2026-10-26T00:01:00Z",
                    project: str = "repo") -> None:
        database = root / "state" / "metrics" / "metrics.db"
        database.parent.mkdir(parents=True)
        import sqlite3
        with sqlite3.connect(database) as connection:
            connection.execute("CREATE TABLE merge_receipts (repo TEXT, project TEXT, base_branch TEXT, imported_at TEXT)")
            connection.execute("INSERT INTO merge_receipts VALUES (?, ?, ?, ?)", ("owner/repo", project, "main", imported_at))
            connection.commit()

    def fake_shipyard(self, directory: pathlib.Path, output: pathlib.Path, *, fail: bool = False, malformed: bool = False) -> pathlib.Path:
        directory.mkdir(parents=True, exist_ok=True)
        executable = directory / "shipyard"
        body = "#!/bin/sh\n"
        if fail:
            body += "echo export failed >&2\nexit 7\n"
        else:
            # The fake accepts the same argv shape and writes the receipt at the
            # value following --file, just as the real export does.
            body += "case \"$*\" in *--version*) echo 'shipyard 0.366.4'; exit 0;; esac\n"
            body += "while [ \"$#\" -gt 0 ]; do if [ \"$1\" = \"--file\" ]; then shift; file=\"$1\"; fi; shift; done\n"
            body += f"printf '%s\\n' '{'not json' if malformed else json.dumps(receipt())}' > \"$file\"\n"
        executable.write_text(body)
        executable.chmod(executable.stat().st_mode | stat.S_IXUSR)
        return executable

    def invoke(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(["python3", str(SCRIPT), *args], cwd=ROOT,
                              text=True, capture_output=True, check=False)

    def remote_envelope(self, *, now: dt.datetime, payload: bytes) -> str:
        return json.dumps({
            "handoff_b64": __import__("base64").b64encode(payload).decode("ascii"),
            "exported_at": now.isoformat().replace("+00:00", "Z"),
            "source_imported_at": now.isoformat().replace("+00:00", "Z"),
            "producer": {"identity": "shipyard", "version": "shipyard 0.378.1", "executable_sha256": "a" * 64},
        })

    def fetch_args(self, root: pathlib.Path, *, source_host: str = "m3") -> object:
        return type("Args", (), {
            "source_host": source_host,
            "source_shipyard": root / "producer" / "shipyard",
            "source_state_dir": root / "producer" / "state",
            "repo": "owner/repo",
            "base_branch": "main",
            "stale_after": "2h",
            "file": root / "observer" / "merge-receipts.jsonl",
            "health_file": root / "observer" / "merge-receipts.health.json",
            "timeout_seconds": 30,
        })()

    def test_export_publishes_atomic_health_sidecar(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            shipyard = self.fake_shipyard(root, root / "handoff.jsonl")
            handoff = root / "handoff.jsonl"
            health = root / "handoff.health.json"
            self.producer_db(root)
            result = self.invoke("export", "--shipyard", str(shipyard), "--state-dir", str(root / "state"),
                              "--repo", "owner/repo", "--file", str(handoff), "--health-file", str(health))
            self.assertEqual(result.returncode, 0, result.stderr)
            value = json.loads(health.read_text())
            self.assertEqual(value["schema"], "shipyard-merge-receipt-handoff/v1")
            self.assertEqual(value["status"], "fresh")
            self.assertEqual(value["receipt_count"], 1)
            self.assertEqual(json.loads(handoff.read_text())["repo"], "owner/repo")

    def test_remote_helper_exports_receipt_and_source_freshness(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            shipyard = self.fake_shipyard(root / "producer", root / "producer" / "handoff.jsonl")
            self.producer_db(root / "producer")
            config = {
                "shipyard": str(shipyard),
                "state_dir": str(root / "producer" / "state"),
                "repo": "owner/repo",
                "base_branch": "main",
                "timeout_seconds": 30,
            }
            encoded = __import__("base64").b64encode(json.dumps(config).encode()).decode()
            result = subprocess.run(["python3", "-", encoded], input=MOD.REMOTE_FETCH_SCRIPT,
                                    text=True, capture_output=True, check=False)
            self.assertEqual(result.returncode, 0, result.stderr)
            envelope = json.loads(result.stdout)
            self.assertIn("handoff_b64", envelope)
            self.assertEqual(envelope["producer"]["identity"], "shipyard")
            self.assertEqual(envelope["source_imported_at"], "2026-10-26T00:01:00Z")

    def test_fetch_validates_remote_payload_and_publishes_observer_health(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            payload = (json.dumps(receipt()) + "\n").encode()
            args = self.fetch_args(root)
            observed = dt.datetime.now(dt.timezone.utc)
            completed = subprocess.CompletedProcess(
                ["ssh"], 0, self.remote_envelope(now=observed, payload=payload), ""
            )
            with mock.patch.object(MOD.subprocess, "run", return_value=completed) as run:
                self.assertEqual(MOD.fetch(args), 0)
            self.assertEqual(json.loads(args.file.read_text())["receipt_id"], "r-1")
            health = json.loads(args.health_file.read_text())
            self.assertEqual(health["status"], "fresh")
            self.assertEqual(health["file"], str(args.file))
            run.assert_called_once()
            command = run.call_args.args[0]
            self.assertEqual(command[:5], ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5"])
            self.assertEqual(command[5], "m3")
            self.assertEqual(command[6:8], ["/usr/bin/python3", "-"])

    def test_fetch_failure_preserves_payload_and_marks_health_error(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            args = self.fetch_args(root)
            args.file.parent.mkdir(parents=True)
            args.file.write_text("previous\n")
            completed = subprocess.CompletedProcess(["ssh"], 255, "", "connection refused")
            with mock.patch.object(MOD.subprocess, "run", return_value=completed):
                with self.assertRaisesRegex(RuntimeError, "connection refused"):
                    MOD.fetch(args)
            self.assertEqual(args.file.read_text(), "previous\n")
            self.assertEqual(json.loads(args.health_file.read_text())["status"], "error")

    def test_fetch_rejects_non_fleet_source_host(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            with self.assertRaisesRegex(ValueError, "plain fleet host"):
                MOD.fetch(self.fetch_args(root, source_host="m3;touch-bad"))

    def test_export_accepts_legacy_mixed_case_project_rows(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            shipyard = self.fake_shipyard(root, root / "handoff.jsonl")
            handoff = root / "handoff.jsonl"
            health = root / "handoff.health.json"
            self.producer_db(root, project="REPO")
            result = self.invoke("export", "--shipyard", str(shipyard), "--state-dir", str(root / "state"),
                                 "--repo", "owner/repo", "--file", str(handoff), "--health-file", str(health))
            self.assertEqual(result.returncode, 0, result.stderr)
            value = json.loads(health.read_text())
            self.assertEqual(value["status"], "fresh")
            self.assertEqual(value["source_imported_at"], "2026-10-26T00:01:00Z")

    def test_export_failure_marks_health_and_does_not_claim_freshness(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            shipyard = self.fake_shipyard(root, root / "handoff.jsonl", fail=True)
            handoff = root / "handoff.jsonl"
            health = root / "handoff.health.json"
            self.producer_db(root)
            handoff.write_text("previous\n")
            result = self.invoke("export", "--shipyard", str(shipyard), "--state-dir", str(root / "state"),
                              "--repo", "owner/repo", "--file", str(handoff), "--health-file", str(health))
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(handoff.read_text(), "previous\n")
            self.assertEqual(json.loads(health.read_text())["status"], "error")

    def test_malformed_staged_export_preserves_previous_payload_and_health(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            handoff = root / "handoff.jsonl"
            health = root / "handoff.health.json"
            handoff.write_text(json.dumps(receipt()) + "\n")
            health.write_text("previous-health\n")
            shipyard = self.fake_shipyard(root, handoff, malformed=True)
            result = self.invoke("export", "--shipyard", str(shipyard), "--state-dir", str(root / "state"),
                                 "--repo", "owner/repo", "--file", str(handoff), "--health-file", str(health))
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(json.loads(handoff.read_text())["receipt_id"], "r-1")
            self.assertEqual(health.read_text(), "previous-health\n")

    def test_check_reports_fresh_then_stale(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            handoff = root / "handoff.jsonl"
            health = root / "handoff.health.json"
            handoff.write_text(json.dumps(receipt()) + "\n")
            now = dt.datetime.now(dt.timezone.utc)
            handoff_payload = handoff.read_bytes()
            health.write_text(json.dumps(self.health(handoff, now=now)))
            fresh = self.invoke("check", "--file", str(handoff), "--repo", "owner/repo", "--health-file", str(health), "--stale-after", "2h")
            self.assertEqual(fresh.returncode, 0, fresh.stdout + fresh.stderr)
            future = json.loads(health.read_text())
            future["exported_at"] = (now + dt.timedelta(hours=1)).isoformat().replace("+00:00", "Z")
            health.write_text(json.dumps(future))
            future_result = self.invoke("check", "--file", str(handoff), "--repo", "owner/repo", "--health-file", str(health), "--stale-after", "2h")
            self.assertEqual(future_result.returncode, 3)
            self.assertIn("future", json.loads(future_result.stdout)["error"])
            bad_producer = json.loads(health.read_text())
            bad_producer["exported_at"] = now.isoformat().replace("+00:00", "Z")
            bad_producer["producer"] = {"identity": 7, "version": "shipyard 0.366.4", "executable_sha256": "a" * 64}
            health.write_text(json.dumps(bad_producer))
            bad_result = self.invoke("check", "--file", str(handoff), "--repo", "owner/repo", "--health-file", str(health), "--stale-after", "2h")
            self.assertEqual(bad_result.returncode, 3)
            self.assertIn("producer", json.loads(bad_result.stdout)["error"])
            old = now - dt.timedelta(hours=3)
            value = json.loads(health.read_text())
            value["exported_at"] = old.isoformat().replace("+00:00", "Z")
            value["producer"] = {"identity": "shipyard", "version": "shipyard 0.366.4", "executable_sha256": "a" * 64}
            health.write_text(json.dumps(value))
            stale = self.invoke("check", "--file", str(handoff), "--repo", "owner/repo", "--health-file", str(health), "--stale-after", "2h")
            self.assertEqual(stale.returncode, 3, stale.stdout + stale.stderr)
            self.assertEqual(json.loads(stale.stdout)["status"], "stale")

    def test_check_rejects_wrong_repository_and_missing_sidecar(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            handoff = root / "handoff.jsonl"
            health = root / "handoff.health.json"
            handoff.write_text(json.dumps(receipt("other/repo")) + "\n")
            result = self.invoke("check", "--file", str(handoff), "--repo", "owner/repo", "--health-file", str(health))
            self.assertEqual(result.returncode, 3)
            self.assertEqual(json.loads(result.stdout)["status"], "error")

    def test_check_rejects_duplicate_and_conflicting_records(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            health = root / "handoff.health.json"
            for second in (receipt(), {**receipt(), "receipt_id": "r-2", "head_sha": "d" * 40,
                                      "validated_head_sha": "d" * 40}):
                handoff = root / "handoff.jsonl"
                handoff.write_text(json.dumps(receipt()) + "\n" + json.dumps(second) + "\n")
                now = dt.datetime.now(dt.timezone.utc)
                sidecar = self.health(handoff, now=now, receipt_count=2)
                sidecar["handoff_bytes"] = len(handoff.read_bytes())
                sidecar["handoff_sha256"] = hashlib.sha256(handoff.read_bytes()).hexdigest()
                health.write_text(json.dumps(sidecar))
                result = self.invoke("check", "--file", str(handoff), "--repo", "owner/repo", "--health-file", str(health))
                self.assertEqual(result.returncode, 3)
                self.assertRegex(json.loads(result.stdout)["error"], "duplicates receipt_id|conflicts")

    def test_check_rejects_minimal_tampered_receipt(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            handoff = root / "handoff.jsonl"
            health = root / "handoff.health.json"
            handoff.write_text(json.dumps({"repo": "owner/repo", "receipt_id": "tampered", "merged_at": "2026-10-26T00:00:00Z"}) + "\n")
            health.write_text(json.dumps(self.health(handoff)))
            handoff.write_text(json.dumps({"repo": "owner/repo", "receipt_id": "tampered", "merged_at": "2026-10-26T00:00:00Z"}) + "\n")
            result = self.invoke("check", "--file", str(handoff), "--repo", "owner/repo", "--health-file", str(health))
            self.assertEqual(result.returncode, 3)
            self.assertIn("fields mismatch", json.loads(result.stdout)["error"])

    def test_check_rejects_sidecar_bound_to_another_file(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            handoff = root / "handoff.jsonl"
            other = root / "other.jsonl"
            health = root / "handoff.health.json"
            handoff.write_text(json.dumps(receipt()) + "\n")
            health.write_text(json.dumps(self.health(handoff, file=str(other))))
            result = self.invoke("check", "--file", str(handoff), "--repo", "owner/repo", "--health-file", str(health))
            self.assertEqual(result.returncode, 3)
            self.assertIn("different handoff file", json.loads(result.stdout)["error"])

    def test_arguments_require_absolute_paths(self) -> None:
        result = self.invoke("check", "--file", "relative", "--repo", "owner/repo", "--health-file", "/tmp/health")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("absolute", result.stderr)

    def test_check_rejects_unknown_health_field(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp); handoff = root / "handoff.jsonl"; health = root / "health.json"
            handoff.write_text(json.dumps(receipt()) + "\n")
            sidecar = self.health(handoff); sidecar["unexpected"] = True; health.write_text(json.dumps(sidecar))
            result = self.invoke("check", "--file", str(handoff), "--repo", "owner/repo", "--health-file", str(health))
            self.assertEqual(result.returncode, 3)
            self.assertIn("unknown fields", json.loads(result.stdout)["error"])

    def test_check_rejects_unknown_producer_field(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp); handoff = root / "handoff.jsonl"; health = root / "health.json"
            handoff.write_text(json.dumps(receipt()) + "\n")
            sidecar = self.health(handoff)
            sidecar["producer"] = {**sidecar["producer"], "unexpected": True}
            health.write_text(json.dumps(sidecar))
            result = self.invoke("check", "--file", str(handoff), "--repo", "owner/repo", "--health-file", str(health))
            self.assertEqual(result.returncode, 3)
            self.assertIn("producer contains unknown fields", json.loads(result.stdout)["error"])

    def test_check_rejects_policy_window_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp); handoff = root / "handoff.jsonl"; health = root / "health.json"
            handoff.write_text(json.dumps(receipt()) + "\n")
            sidecar = self.health(handoff, stale_after_seconds=3600); health.write_text(json.dumps(sidecar))
            result = self.invoke("check", "--file", str(handoff), "--repo", "owner/repo", "--health-file", str(health), "--stale-after", "2h")
            self.assertEqual(result.returncode, 3)
            self.assertIn("freshness window", json.loads(result.stdout)["error"])

    def test_check_requires_freshness_metadata_and_matches_latest_receipt(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp); handoff = root / "handoff.jsonl"; health = root / "health.json"
            handoff.write_text(json.dumps(receipt()) + "\n")
            for field in ("checked_at", "latest_merged_at", "source_imported_at"):
                sidecar = self.health(handoff); sidecar.pop(field); health.write_text(json.dumps(sidecar))
                result = self.invoke("check", "--file", str(handoff), "--repo", "owner/repo", "--health-file", str(health))
                self.assertEqual(result.returncode, 3)
                self.assertIn("missing required fields", json.loads(result.stdout)["error"])
            sidecar = self.health(handoff, latest_merged_at="2026-10-26T00:02:00Z")
            health.write_text(json.dumps(sidecar))
            result = self.invoke("check", "--file", str(handoff), "--repo", "owner/repo", "--health-file", str(health))
            self.assertEqual(result.returncode, 3)
            self.assertIn("latest_merged_at", json.loads(result.stdout)["error"])
            old = dt.datetime.now(dt.timezone.utc) - dt.timedelta(hours=3)
            sidecar = self.health(handoff, source_imported_at=old.isoformat().replace("+00:00", "Z"))
            health.write_text(json.dumps(sidecar))
            result = self.invoke("check", "--file", str(handoff), "--repo", "owner/repo", "--health-file", str(health), "--stale-after", "2h")
            self.assertEqual(result.returncode, 3)
            self.assertEqual(json.loads(result.stdout)["status"], "stale")

    def test_check_rejects_wrong_project_or_base_branch(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp); handoff = root / "handoff.jsonl"; health = root / "health.json"
            handoff.write_text(json.dumps(receipt()) + "\n")
            for field, value, message in (("project", "wrong", "project"), ("base_branch", "develop", "base branch")):
                sidecar = self.health(handoff, **{field: value}); health.write_text(json.dumps(sidecar))
                result = self.invoke("check", "--file", str(handoff), "--repo", "owner/repo", "--health-file", str(health))
                self.assertEqual(result.returncode, 3)
                self.assertIn(message, json.loads(result.stdout)["error"])


if __name__ == "__main__":
    unittest.main()

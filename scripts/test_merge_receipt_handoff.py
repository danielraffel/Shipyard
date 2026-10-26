#!/usr/bin/env python3
from __future__ import annotations

import datetime as dt
import hashlib
import json
import os
import pathlib
import stat
import subprocess
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "merge_receipt_handoff.py"


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
    def fake_shipyard(self, directory: pathlib.Path, output: pathlib.Path, *, fail: bool = False) -> pathlib.Path:
        executable = directory / "shipyard"
        body = "#!/bin/sh\n"
        if fail:
            body += "echo export failed >&2\nexit 7\n"
        else:
            # The fake accepts the same argv shape and writes the receipt at the
            # value following --file, just as the real export does.
            body += "while [ \"$#\" -gt 0 ]; do if [ \"$1\" = \"--file\" ]; then shift; file=\"$1\"; fi; shift; done\n"
            body += f"printf '%s\\n' '{json.dumps(receipt())}' > \"$file\"\n"
        executable.write_text(body)
        executable.chmod(executable.stat().st_mode | stat.S_IXUSR)
        return executable

    def invoke(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(["python3", str(SCRIPT), *args], cwd=ROOT,
                              text=True, capture_output=True, check=False)

    def test_export_publishes_atomic_health_sidecar(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            shipyard = self.fake_shipyard(root, root / "handoff.jsonl")
            handoff = root / "handoff.jsonl"
            health = root / "handoff.health.json"
            result = self.invoke("export", "--shipyard", str(shipyard), "--state-dir", str(root / "state"),
                              "--repo", "owner/repo", "--file", str(handoff), "--health-file", str(health))
            self.assertEqual(result.returncode, 0, result.stderr)
            value = json.loads(health.read_text())
            self.assertEqual(value["schema"], "shipyard-merge-receipt-handoff/v1")
            self.assertEqual(value["status"], "fresh")
            self.assertEqual(value["receipt_count"], 1)
            self.assertEqual(json.loads(handoff.read_text())["repo"], "owner/repo")

    def test_export_failure_marks_health_and_does_not_claim_freshness(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            shipyard = self.fake_shipyard(root, root / "handoff.jsonl", fail=True)
            handoff = root / "handoff.jsonl"
            health = root / "handoff.health.json"
            handoff.write_text("previous\n")
            result = self.invoke("export", "--shipyard", str(shipyard), "--state-dir", str(root / "state"),
                              "--repo", "owner/repo", "--file", str(handoff), "--health-file", str(health))
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(handoff.read_text(), "previous\n")
            self.assertEqual(json.loads(health.read_text())["status"], "error")

    def test_check_reports_fresh_then_stale(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            handoff = root / "handoff.jsonl"
            health = root / "handoff.health.json"
            handoff.write_text(json.dumps(receipt()) + "\n")
            now = dt.datetime.now(dt.timezone.utc)
            handoff_payload = handoff.read_bytes()
            health.write_text(json.dumps({
                "schema": "shipyard-merge-receipt-handoff/v1", "repo": "owner/repo",
                "file": str(handoff), "status": "fresh", "checked_at": now.isoformat().replace("+00:00", "Z"),
                "exported_at": now.isoformat().replace("+00:00", "Z"), "receipt_count": 1,
                "handoff_bytes": len(handoff_payload), "handoff_sha256": hashlib.sha256(handoff_payload).hexdigest(),
                "producer": {"identity": "shipyard", "version": "unavailable", "executable_sha256": "unavailable"},
            }))
            fresh = self.invoke("check", "--file", str(handoff), "--health-file", str(health), "--stale-after", "2h")
            self.assertEqual(fresh.returncode, 0, fresh.stdout + fresh.stderr)
            old = now - dt.timedelta(hours=3)
            value = json.loads(health.read_text())
            value["exported_at"] = old.isoformat().replace("+00:00", "Z")
            health.write_text(json.dumps(value))
            stale = self.invoke("check", "--file", str(handoff), "--health-file", str(health), "--stale-after", "2h")
            self.assertEqual(stale.returncode, 3, stale.stdout + stale.stderr)
            self.assertEqual(json.loads(stale.stdout)["status"], "stale")

    def test_check_rejects_wrong_repository_and_missing_sidecar(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            handoff = root / "handoff.jsonl"
            health = root / "handoff.health.json"
            handoff.write_text(json.dumps(receipt("other/repo")) + "\n")
            result = self.invoke("check", "--file", str(handoff), "--health-file", str(health))
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
                health.write_text(json.dumps({"schema": "shipyard-merge-receipt-handoff/v1",
                    "repo": "owner/repo", "file": str(handoff), "status": "fresh",
                    "exported_at": now.isoformat().replace("+00:00", "Z"),
                    "handoff_bytes": len(handoff.read_bytes()),
                    "handoff_sha256": hashlib.sha256(handoff.read_bytes()).hexdigest(),
                    "receipt_count": 2,
                    "producer": {"identity": "shipyard", "version": "unavailable", "executable_sha256": "unavailable"}}))
                result = self.invoke("check", "--file", str(handoff), "--health-file", str(health))
                self.assertEqual(result.returncode, 3)
                self.assertRegex(json.loads(result.stdout)["error"], "duplicates receipt_id|conflicts")

    def test_check_rejects_minimal_tampered_receipt(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            handoff = root / "handoff.jsonl"
            health = root / "handoff.health.json"
            handoff.write_text(json.dumps({"repo": "owner/repo", "receipt_id": "tampered", "merged_at": "2026-10-26T00:00:00Z"}) + "\n")
            health.write_text(json.dumps({
                "schema": "shipyard-merge-receipt-handoff/v1", "repo": "owner/repo",
                "file": str(handoff), "status": "fresh",
                "exported_at": dt.datetime.now(dt.timezone.utc).isoformat().replace("+00:00", "Z"),
            }))
            result = self.invoke("check", "--file", str(handoff), "--health-file", str(health))
            self.assertEqual(result.returncode, 3)
            self.assertIn("fields mismatch", json.loads(result.stdout)["error"])

    def test_check_rejects_sidecar_bound_to_another_file(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            handoff = root / "handoff.jsonl"
            other = root / "other.jsonl"
            health = root / "handoff.health.json"
            handoff.write_text(json.dumps(receipt()) + "\n")
            health.write_text(json.dumps({
                "schema": "shipyard-merge-receipt-handoff/v1", "repo": "owner/repo",
                "file": str(other), "status": "fresh",
                "exported_at": dt.datetime.now(dt.timezone.utc).isoformat().replace("+00:00", "Z"),
            }))
            result = self.invoke("check", "--file", str(handoff), "--health-file", str(health))
            self.assertEqual(result.returncode, 3)
            self.assertIn("different handoff file", json.loads(result.stdout)["error"])

    def test_arguments_require_absolute_paths(self) -> None:
        result = self.invoke("check", "--file", "relative", "--health-file", "/tmp/health")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("absolute", result.stderr)


if __name__ == "__main__":
    unittest.main()

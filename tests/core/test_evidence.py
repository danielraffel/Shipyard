"""Tests for evidence tracking."""

from __future__ import annotations

from datetime import datetime, timezone

from shipyard.core.evidence import EvidenceRecord, EvidenceStore


class TestEvidenceRecord:
    def test_passed(self) -> None:
        rec = EvidenceRecord(
            sha="abc", branch="main", target_name="mac",
            platform="macos-arm64", status="pass", backend="local",
            completed_at=datetime.now(timezone.utc),
        )
        assert rec.passed

    def test_failed(self) -> None:
        rec = EvidenceRecord(
            sha="abc", branch="main", target_name="mac",
            platform="macos-arm64", status="fail", backend="local",
            completed_at=datetime.now(timezone.utc),
        )
        assert not rec.passed

    def test_roundtrip(self) -> None:
        now = datetime.now(timezone.utc)
        rec = EvidenceRecord(
            sha="abc", branch="feat/x", target_name="ubuntu",
            platform="linux-x64", status="pass", backend="ssh",
            completed_at=now, duration_secs=120.5, host="192.168.1.10",
        )
        d = rec.to_dict()
        restored = EvidenceRecord.from_dict(d)
        assert restored.sha == rec.sha
        assert restored.target_name == rec.target_name
        assert restored.duration_secs == rec.duration_secs

    def test_failover_fields(self) -> None:
        rec = EvidenceRecord(
            sha="abc", branch="main", target_name="ubuntu",
            platform="linux-x64", status="pass", backend="namespace-failover",
            completed_at=datetime.now(timezone.utc),
            primary_backend="ssh", failover_reason="ssh_unreachable",
            provider="namespace", runner_profile="namespace-profile-default",
        )
        d = rec.to_dict()
        assert d["primary_backend"] == "ssh"
        assert d["failover_reason"] == "ssh_unreachable"


class TestEvidenceStore:
    def test_record_and_retrieve(self, evidence_store: EvidenceStore) -> None:
        rec = EvidenceRecord(
            sha="abc", branch="feat/x", target_name="mac",
            platform="macos-arm64", status="pass", backend="local",
            completed_at=datetime.now(timezone.utc),
        )
        evidence_store.record(rec)
        retrieved = evidence_store.get_target("feat/x", "mac")
        assert retrieved is not None
        assert retrieved.sha == "abc"

    def test_latest_overwrites(self, evidence_store: EvidenceStore) -> None:
        old = EvidenceRecord(
            sha="old", branch="main", target_name="mac",
            platform="macos-arm64", status="pass", backend="local",
            completed_at=datetime.now(timezone.utc),
        )
        new = EvidenceRecord(
            sha="new", branch="main", target_name="mac",
            platform="macos-arm64", status="pass", backend="local",
            completed_at=datetime.now(timezone.utc),
        )
        evidence_store.record(old)
        evidence_store.record(new)
        retrieved = evidence_store.get_target("main", "mac")
        assert retrieved is not None
        assert retrieved.sha == "new"

    def test_get_branch(self, evidence_store: EvidenceStore) -> None:
        for name in ("mac", "ubuntu", "windows"):
            evidence_store.record(EvidenceRecord(
                sha="abc", branch="feat/x", target_name=name,
                platform=f"{name}-platform", status="pass", backend="local",
                completed_at=datetime.now(timezone.utc),
            ))
        records = evidence_store.get_branch("feat/x")
        assert len(records) == 3

    def test_merge_ready_all_green(self, evidence_store: EvidenceStore) -> None:
        for name, platform in [("mac", "macos-arm64"), ("ubuntu", "linux-x64")]:
            evidence_store.record(EvidenceRecord(
                sha="abc", branch="main", target_name=name,
                platform=platform, status="pass", backend="local",
                completed_at=datetime.now(timezone.utc),
            ))
        ready, evidence_map = evidence_store.is_merge_ready(
            "main", "abc", ["macos-arm64", "linux-x64"],
        )
        assert ready
        assert all(v is not None for v in evidence_map.values())

    def test_merge_not_ready_missing_platform(self, evidence_store: EvidenceStore) -> None:
        evidence_store.record(EvidenceRecord(
            sha="abc", branch="main", target_name="mac",
            platform="macos-arm64", status="pass", backend="local",
            completed_at=datetime.now(timezone.utc),
        ))
        ready, evidence_map = evidence_store.is_merge_ready(
            "main", "abc", ["macos-arm64", "linux-x64"],
        )
        assert not ready
        assert evidence_map["linux-x64"] is None

    def test_merge_not_ready_wrong_sha(self, evidence_store: EvidenceStore) -> None:
        evidence_store.record(EvidenceRecord(
            sha="old", branch="main", target_name="mac",
            platform="macos-arm64", status="pass", backend="local",
            completed_at=datetime.now(timezone.utc),
        ))
        ready, _ = evidence_store.is_merge_ready("main", "new", ["macos-arm64"])
        assert not ready

    def test_merge_not_ready_failed(self, evidence_store: EvidenceStore) -> None:
        evidence_store.record(EvidenceRecord(
            sha="abc", branch="main", target_name="mac",
            platform="macos-arm64", status="fail", backend="local",
            completed_at=datetime.now(timezone.utc),
        ))
        ready, _ = evidence_store.is_merge_ready("main", "abc", ["macos-arm64"])
        assert not ready

    def test_persistence(self, tmp_path: object, evidence_store: EvidenceStore) -> None:
        evidence_store.record(EvidenceRecord(
            sha="abc", branch="main", target_name="mac",
            platform="macos-arm64", status="pass", backend="local",
            completed_at=datetime.now(timezone.utc),
        ))
        # Create a new store pointing to the same path
        store2 = EvidenceStore(path=evidence_store.path)
        retrieved = store2.get_target("main", "mac")
        assert retrieved is not None
        assert retrieved.sha == "abc"

    def test_empty_branch(self, evidence_store: EvidenceStore) -> None:
        records = evidence_store.get_branch("nonexistent")
        assert records == {}

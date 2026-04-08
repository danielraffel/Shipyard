"""Tests for merge-on-green logic."""

from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from shipyard.core.config import Config
from shipyard.core.evidence import EvidenceRecord, EvidenceStore
from shipyard.core.queue import Queue
from shipyard.ship.merge import MergeCheck, ShipResult, can_merge, ship


class TestCanMerge:
    def test_all_platforms_green(self, evidence_store: EvidenceStore) -> None:
        sha = "abc123"
        branch = "feature/x"
        for name, platform in [("mac", "macos-arm64"), ("ubuntu", "linux-x64")]:
            evidence_store.record(EvidenceRecord(
                sha=sha, branch=branch, target_name=name,
                platform=platform, status="pass", backend="local",
                completed_at=datetime.now(timezone.utc),
            ))

        check = can_merge(evidence_store, branch, sha, ["macos-arm64", "linux-x64"])
        assert check.ready
        assert len(check.passing) == 2
        assert len(check.missing) == 0
        assert len(check.failing) == 0

    def test_missing_platform(self, evidence_store: EvidenceStore) -> None:
        sha = "abc123"
        branch = "feature/x"
        evidence_store.record(EvidenceRecord(
            sha=sha, branch=branch, target_name="mac",
            platform="macos-arm64", status="pass", backend="local",
            completed_at=datetime.now(timezone.utc),
        ))

        check = can_merge(evidence_store, branch, sha, ["macos-arm64", "linux-x64"])
        assert not check.ready
        assert check.passing == ["macos-arm64"]
        assert check.missing == ["linux-x64"]

    def test_failed_platform(self, evidence_store: EvidenceStore) -> None:
        sha = "abc123"
        branch = "feature/x"
        evidence_store.record(EvidenceRecord(
            sha=sha, branch=branch, target_name="mac",
            platform="macos-arm64", status="fail", backend="local",
            completed_at=datetime.now(timezone.utc),
        ))

        check = can_merge(evidence_store, branch, sha, ["macos-arm64"])
        assert not check.ready
        assert check.failing == ["macos-arm64"]

    def test_wrong_sha(self, evidence_store: EvidenceStore) -> None:
        evidence_store.record(EvidenceRecord(
            sha="old_sha", branch="feature/x", target_name="mac",
            platform="macos-arm64", status="pass", backend="local",
            completed_at=datetime.now(timezone.utc),
        ))

        check = can_merge(evidence_store, branch="feature/x", sha="new_sha",
                          required_platforms=["macos-arm64"])
        assert not check.ready
        assert check.missing == ["macos-arm64"]

    def test_empty_evidence(self, evidence_store: EvidenceStore) -> None:
        check = can_merge(evidence_store, "feature/x", "abc",
                          required_platforms=["macos-arm64", "linux-x64"])
        assert not check.ready
        assert len(check.missing) == 2


class TestMergeCheck:
    def test_to_dict(self) -> None:
        check = MergeCheck(
            ready=False,
            sha="abc",
            branch="feature/x",
            required_platforms=["macos-arm64", "linux-x64"],
            passing=["macos-arm64"],
            missing=["linux-x64"],
            failing=[],
        )
        d = check.to_dict()
        assert d["ready"] is False
        assert d["passing"] == ["macos-arm64"]
        assert d["missing"] == ["linux-x64"]


class TestShipResult:
    def test_success_to_dict(self) -> None:
        from shipyard.ship.pr import PrInfo
        result = ShipResult(
            success=True,
            pr=PrInfo(number=1, url="u", title="t", state="MERGED",
                      branch="b", base="main"),
            merge_check=MergeCheck(
                ready=True, sha="a", branch="b",
                required_platforms=["macos-arm64"],
                passing=["macos-arm64"], missing=[], failing=[],
            ),
        )
        d = result.to_dict()
        assert d["success"] is True
        assert "pr" in d
        assert "merge_check" in d

    def test_failure_to_dict(self) -> None:
        result = ShipResult(success=False, error="Not in a git repo")
        d = result.to_dict()
        assert d["success"] is False
        assert d["error"] == "Not in a git repo"


class TestShip:
    @patch("shipyard.ship.merge._git_branch", return_value="main")
    @patch("shipyard.ship.merge._git_sha", return_value="abc123")
    def test_refuses_main_branch(
        self, mock_sha: MagicMock, mock_branch: MagicMock,
        tmp_path: Path,
    ) -> None:
        config = Config(data={"merge": {"require_platforms": ["macos-arm64"]}})
        queue = Queue(state_dir=tmp_path / "queue")
        evidence = EvidenceStore(path=tmp_path / "evidence")

        result = ship(config, queue, evidence)
        assert not result.success
        assert "main" in (result.error or "")

    @patch("shipyard.ship.merge._git_branch", return_value=None)
    @patch("shipyard.ship.merge._git_sha", return_value=None)
    def test_refuses_non_git(
        self, mock_sha: MagicMock, mock_branch: MagicMock,
        tmp_path: Path,
    ) -> None:
        config = Config(data={})
        queue = Queue(state_dir=tmp_path / "queue")
        evidence = EvidenceStore(path=tmp_path / "evidence")

        result = ship(config, queue, evidence)
        assert not result.success
        assert "git" in (result.error or "").lower()

    @patch("shipyard.ship.merge._git_branch", return_value="feature/x")
    @patch("shipyard.ship.merge._git_sha", return_value="abc123")
    def test_refuses_no_required_platforms(
        self, mock_sha: MagicMock, mock_branch: MagicMock,
        tmp_path: Path,
    ) -> None:
        config = Config(data={"merge": {}})
        queue = Queue(state_dir=tmp_path / "queue")
        evidence = EvidenceStore(path=tmp_path / "evidence")

        result = ship(config, queue, evidence)
        assert not result.success
        assert "platforms" in (result.error or "").lower()

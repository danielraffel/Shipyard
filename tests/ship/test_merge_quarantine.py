"""Tests for quarantine-aware merge decisions.

Quarantine suppresses TEST/UNKNOWN failures on quarantined targets but
keeps INFRA/TIMEOUT/CONTRACT authoritative. These are the merge-layer
semantics — the rest of the ship flow doesn't need to care.
"""

from __future__ import annotations

from datetime import datetime, timezone

import pytest

from shipyard.core.evidence import EvidenceRecord, EvidenceStore
from shipyard.core.quarantine import QuarantineEntry, QuarantineList
from shipyard.ship.merge import can_merge


@pytest.fixture
def empty_quarantine() -> QuarantineList:
    return QuarantineList(entries=[], path=None)


@pytest.fixture
def flaky_quarantine() -> QuarantineList:
    return QuarantineList(
        entries=[QuarantineEntry(target="flaky-win", reason="bad runner")],
        path=None,
    )


def _record(
    store: EvidenceStore,
    *,
    target: str,
    platform: str,
    status: str,
    failure_class: str | None = None,
    sha: str = "abc123",
    branch: str = "feature/x",
) -> None:
    store.record(EvidenceRecord(
        sha=sha,
        branch=branch,
        target_name=target,
        platform=platform,
        status=status,
        backend="local",
        completed_at=datetime.now(timezone.utc),
        failure_class=failure_class,
    ))


class TestQuarantineSuppressesTestFailures:
    def test_quarantined_target_test_failure_is_advisory(
        self,
        evidence_store: EvidenceStore,
        flaky_quarantine: QuarantineList,
    ) -> None:
        _record(
            evidence_store, target="mac", platform="macos-arm64",
            status="pass",
        )
        _record(
            evidence_store, target="flaky-win", platform="windows-arm64",
            status="fail", failure_class="TEST",
        )

        check = can_merge(
            evidence_store, "feature/x", "abc123",
            ["macos-arm64", "windows-arm64"],
            quarantine=flaky_quarantine,
        )

        assert check.ready is True
        assert "windows-arm64" in check.advisory
        assert "windows-arm64" not in check.failing
        assert "macos-arm64" in check.passing

    def test_quarantined_target_unknown_failure_is_advisory(
        self,
        evidence_store: EvidenceStore,
        flaky_quarantine: QuarantineList,
    ) -> None:
        _record(
            evidence_store, target="flaky-win", platform="windows-arm64",
            status="fail", failure_class="UNKNOWN",
        )

        check = can_merge(
            evidence_store, "feature/x", "abc123",
            ["windows-arm64"],
            quarantine=flaky_quarantine,
        )
        assert check.ready is True
        assert check.advisory == ["windows-arm64"]


class TestQuarantineNeverSuppressesAuthoritativeClasses:
    @pytest.mark.parametrize(
        "failure_class",
        ["INFRA", "TIMEOUT", "CONTRACT"],
    )
    def test_authoritative_classes_still_block(
        self,
        evidence_store: EvidenceStore,
        flaky_quarantine: QuarantineList,
        failure_class: str,
    ) -> None:
        _record(
            evidence_store, target="flaky-win", platform="windows-arm64",
            status="fail", failure_class=failure_class,
        )

        check = can_merge(
            evidence_store, "feature/x", "abc123",
            ["windows-arm64"],
            quarantine=flaky_quarantine,
        )
        assert check.ready is False
        assert check.failing == ["windows-arm64"]
        assert check.advisory == []


class TestNoQuarantineIsIdentityBehavior:
    def test_passing_empty_list_matches_old_behavior(
        self,
        evidence_store: EvidenceStore,
    ) -> None:
        _record(
            evidence_store, target="mac", platform="macos-arm64",
            status="fail", failure_class="TEST",
        )
        # No quarantine arg at all.
        check = can_merge(
            evidence_store, "feature/x", "abc123",
            ["macos-arm64"],
        )
        assert check.ready is False
        assert check.failing == ["macos-arm64"]
        assert check.advisory == []

    def test_quarantined_unrelated_target_does_nothing(
        self,
        evidence_store: EvidenceStore,
    ) -> None:
        q = QuarantineList(
            entries=[QuarantineEntry(target="different-target")],
            path=None,
        )
        _record(
            evidence_store, target="mac", platform="macos-arm64",
            status="fail", failure_class="TEST",
        )
        check = can_merge(
            evidence_store, "feature/x", "abc123",
            ["macos-arm64"],
            quarantine=q,
        )
        assert check.ready is False
        assert check.failing == ["macos-arm64"]


class TestMergeCheckDictContainsAdvisory:
    def test_to_dict_emits_advisory_when_present(
        self,
        evidence_store: EvidenceStore,
        flaky_quarantine: QuarantineList,
    ) -> None:
        _record(
            evidence_store, target="flaky-win", platform="windows-arm64",
            status="fail", failure_class="TEST",
        )
        check = can_merge(
            evidence_store, "feature/x", "abc123",
            ["windows-arm64"],
            quarantine=flaky_quarantine,
        )
        d = check.to_dict()
        assert d["advisory"] == ["windows-arm64"]
        assert d["failing"] == []
        assert d["ready"] is True

    def test_to_dict_omits_advisory_when_empty(
        self,
        evidence_store: EvidenceStore,
    ) -> None:
        _record(
            evidence_store, target="mac", platform="macos-arm64",
            status="pass",
        )
        check = can_merge(
            evidence_store, "feature/x", "abc123",
            ["macos-arm64"],
        )
        d = check.to_dict()
        assert "advisory" not in d

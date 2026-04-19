"""Tests for the flaky-target quarantine list."""

from __future__ import annotations

from pathlib import Path

from shipyard.core.quarantine import (
    QUARANTINE_FILENAME,
    QuarantineEntry,
    QuarantineList,
    is_advisory_failure,
)


class TestLoad:
    def test_missing_file_is_empty(self, tmp_path: Path) -> None:
        q = QuarantineList.load(tmp_path / "missing.toml")
        assert q.entries == []

    def test_none_path_is_empty(self) -> None:
        q = QuarantineList.load(None)
        assert q.entries == []

    def test_roundtrip(self, tmp_path: Path) -> None:
        path = tmp_path / QUARANTINE_FILENAME
        q = QuarantineList(entries=[], path=path)
        assert q.add("windows-arm64", reason="flaky runner") is True
        q.save()

        reloaded = QuarantineList.load(path)
        assert len(reloaded.entries) == 1
        entry = reloaded.entries[0]
        assert entry.target == "windows-arm64"
        assert entry.reason == "flaky runner"
        assert entry.added_at  # today's date was populated

    def test_malformed_entries_are_dropped(self, tmp_path: Path) -> None:
        path = tmp_path / QUARANTINE_FILENAME
        path.write_text(
            "[[quarantine]]\ntarget = \"valid\"\n"
            "[[quarantine]]\nreason = \"no target key\"\n"
        )
        q = QuarantineList.load(path)
        assert [e.target for e in q.entries] == ["valid"]


class TestAddRemove:
    def test_add_is_idempotent(self, tmp_path: Path) -> None:
        q = QuarantineList(entries=[], path=tmp_path / "q.toml")
        assert q.add("foo") is True
        assert q.add("foo") is False
        assert len(q.entries) == 1

    def test_remove_returns_false_when_absent(self) -> None:
        q = QuarantineList(entries=[QuarantineEntry(target="foo")])
        assert q.remove("bar") is False
        assert q.remove("foo") is True
        assert q.entries == []


class TestIsAdvisory:
    def _q(self, *targets: str) -> QuarantineList:
        return QuarantineList(
            entries=[QuarantineEntry(target=t) for t in targets]
        )

    def test_quarantined_test_failure_is_advisory(self) -> None:
        assert is_advisory_failure(self._q("flaky"), "flaky", "TEST") is True

    def test_quarantined_unknown_failure_is_advisory(self) -> None:
        assert (
            is_advisory_failure(self._q("flaky"), "flaky", "UNKNOWN") is True
        )

    def test_quarantined_infra_still_blocks(self) -> None:
        assert is_advisory_failure(self._q("flaky"), "flaky", "INFRA") is False

    def test_quarantined_timeout_still_blocks(self) -> None:
        assert (
            is_advisory_failure(self._q("flaky"), "flaky", "TIMEOUT") is False
        )

    def test_quarantined_contract_still_blocks(self) -> None:
        assert (
            is_advisory_failure(self._q("flaky"), "flaky", "CONTRACT") is False
        )

    def test_not_quarantined_never_advisory(self) -> None:
        assert is_advisory_failure(self._q(), "anything", "TEST") is False

    def test_missing_class_never_advisory(self) -> None:
        assert is_advisory_failure(self._q("flaky"), "flaky", None) is False

    def test_save_without_path_raises(self) -> None:
        import pytest

        q = QuarantineList(entries=[QuarantineEntry(target="foo")], path=None)
        with pytest.raises(ValueError):
            q.save()

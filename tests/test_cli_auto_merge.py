"""Tests for `shipyard auto-merge`."""

from __future__ import annotations

from datetime import datetime, timezone
from typing import TYPE_CHECKING, Any

from click.testing import CliRunner

from shipyard.cli import main
from shipyard.core.ship_state import ShipState, ShipStateStore

if TYPE_CHECKING:
    from pathlib import Path

    import pytest


def _state(pr: int = 42, evidence: dict[str, str] | None = None) -> ShipState:
    now = datetime.now(timezone.utc)
    return ShipState(
        pr=pr,
        repo="owner/repo",
        branch="feature/x",
        base_branch="main",
        head_sha="a" * 40,
        policy_signature="p1",
        evidence_snapshot=evidence or {},
        created_at=now,
        updated_at=now,
    )


def _runner_with_store(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> tuple[CliRunner, ShipStateStore]:
    store = ShipStateStore(path=tmp_path / "ship")
    monkeypatch.setattr(
        "shipyard.cli.Context.ship_state",
        property(lambda self: store),
    )
    return CliRunner(), store


class TestAutoMerge:
    def test_missing_pr_exits_2(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        runner, _ = _runner_with_store(tmp_path, monkeypatch)
        result = runner.invoke(main, ["auto-merge", "999"])
        assert result.exit_code == 2
        assert "no ship state found" in result.output.lower()

    def test_in_flight_exits_3(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        runner, store = _runner_with_store(tmp_path, monkeypatch)
        store.save(_state(pr=10, evidence={"macos": "pending"}))
        result = runner.invoke(main, ["auto-merge", "10"])
        assert result.exit_code == 3
        assert "in flight" in result.output.lower()

    def test_target_failed_exits_1(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        runner, store = _runner_with_store(tmp_path, monkeypatch)
        store.save(
            _state(pr=11, evidence={"macos": "pass", "linux": "fail"})
        )
        result = runner.invoke(main, ["auto-merge", "11"])
        assert result.exit_code == 1
        assert "linux" in result.output.lower()
        assert "failed" in result.output.lower()

    def test_all_green_merges_and_archives(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        runner, store = _runner_with_store(tmp_path, monkeypatch)
        store.save(
            _state(pr=12, evidence={"macos": "pass", "linux": "pass"})
        )
        calls: list[dict[str, Any]] = []

        def fake_merge(pr_number, *, method="merge",
                       delete_branch=True, admin=False):
            calls.append({
                "pr": pr_number,
                "method": method,
                "delete_branch": delete_branch,
                "admin": admin,
            })
            return object()  # truthy return signals success

        monkeypatch.setattr("shipyard.ship.pr.merge_pr", fake_merge)

        result = runner.invoke(main, ["auto-merge", "12"])
        assert result.exit_code == 0, result.output
        assert calls == [{
            "pr": 12,
            "method": "squash",
            "delete_branch": True,
            "admin": False,
        }]
        # State was archived on success so re-runs exit clean.
        assert store.get(12) is None

    def test_admin_flag_forwarded(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        runner, store = _runner_with_store(tmp_path, monkeypatch)
        store.save(_state(pr=13, evidence={"macos": "pass"}))
        calls: list[dict[str, Any]] = []

        def fake_merge(pr_number, **kw):
            calls.append(kw)
            return object()

        monkeypatch.setattr("shipyard.ship.pr.merge_pr", fake_merge)

        result = runner.invoke(
            main,
            ["auto-merge", "13", "--admin", "--merge-method", "rebase"],
        )
        assert result.exit_code == 0, result.output
        assert calls[0]["admin"] is True
        assert calls[0]["method"] == "rebase"

    def test_merge_returns_falsy_exits_1(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        runner, store = _runner_with_store(tmp_path, monkeypatch)
        store.save(_state(pr=14, evidence={"macos": "pass"}))

        monkeypatch.setattr(
            "shipyard.ship.pr.merge_pr",
            lambda pr_number, **kw: None,
        )

        result = runner.invoke(main, ["auto-merge", "14"])
        assert result.exit_code == 1
        assert "failed" in result.output.lower()
        # State should NOT be archived on failed merge.
        assert store.get(14) is not None

    def test_json_output_shape(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        runner, store = _runner_with_store(tmp_path, monkeypatch)
        store.save(_state(pr=15, evidence={"macos": "pass"}))

        monkeypatch.setattr(
            "shipyard.ship.pr.merge_pr",
            lambda pr_number, **kw: object(),
        )

        result = runner.invoke(
            main, ["--json", "auto-merge", "15"]
        )
        assert result.exit_code == 0
        assert '"event": "merged"' in result.output
        assert '"pr": 15' in result.output

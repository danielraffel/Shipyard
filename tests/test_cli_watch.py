"""Tests for `shipyard watch`.

Uses monkeypatched ship state + branch detection so the tests are
hermetic — no git interaction, no sleeping, no real state dir.
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone
from typing import TYPE_CHECKING

from click.testing import CliRunner

from shipyard.cli import (
    _ship_terminal_verdict,
    _watch_signature,
    main,
)
from shipyard.core.ship_state import (
    DispatchedRun,
    ShipState,
    ShipStateStore,
)

if TYPE_CHECKING:
    from pathlib import Path

    import pytest


def _state(
    pr: int = 42,
    evidence: dict[str, str] | None = None,
    runs: list[DispatchedRun] | None = None,
) -> ShipState:
    now = datetime.now(timezone.utc)
    return ShipState(
        pr=pr,
        repo="owner/repo",
        branch="feature/x",
        base_branch="main",
        head_sha="a" * 40,
        policy_signature="p1",
        evidence_snapshot=evidence or {},
        dispatched_runs=runs or [],
        created_at=now,
        updated_at=now,
    )


def _run(
    target: str = "mac", status: str = "in_progress", run_id: str = "42"
) -> DispatchedRun:
    now = datetime.now(timezone.utc)
    return DispatchedRun(
        target=target,
        provider="local",
        run_id=run_id,
        status=status,
        started_at=now,
        updated_at=now,
    )


class TestSignature:
    def test_stable_when_nothing_changed(self) -> None:
        s = _state(evidence={"macos": "pass"})
        # Two snapshots of the same state must produce identical
        # signatures, even if `updated_at` later drifted.
        sig_a = _watch_signature(s)
        s.updated_at = s.updated_at + timedelta(seconds=5)
        sig_b = _watch_signature(s)
        assert sig_a == sig_b

    def test_changes_on_evidence_update(self) -> None:
        s = _state(evidence={"macos": "pass"})
        before = _watch_signature(s)
        s.update_evidence("linux", "pass")
        after = _watch_signature(s)
        assert before != after

    def test_changes_on_run_status_update(self) -> None:
        s = _state(runs=[_run(status="in_progress")])
        before = _watch_signature(s)
        s.dispatched_runs = [_run(status="completed")]
        after = _watch_signature(s)
        assert before != after


class TestTerminalVerdict:
    def test_all_pass_is_success(self) -> None:
        s = _state(evidence={"macos": "pass", "linux": "pass"})
        assert _ship_terminal_verdict(s) is True

    def test_any_fail_is_failure(self) -> None:
        s = _state(evidence={"macos": "pass", "linux": "fail"})
        assert _ship_terminal_verdict(s) is False

    def test_pending_is_in_flight(self) -> None:
        s = _state(evidence={"macos": "pass", "linux": "pending"})
        assert _ship_terminal_verdict(s) is None

    def test_empty_evidence_is_in_flight(self) -> None:
        assert _ship_terminal_verdict(_state()) is None


class TestWatchCli:
    def _runner_with_store(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> tuple[CliRunner, ShipStateStore]:
        store = ShipStateStore(path=tmp_path / "ship")
        monkeypatch.setattr(
            "shipyard.cli.Context.ship_state",
            property(lambda self: store),
        )
        # Don't actually sleep.
        import time

        monkeypatch.setattr(time, "sleep", lambda s: None)
        return CliRunner(), store

    def test_no_active_ship_exits_2(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        runner, _ = self._runner_with_store(tmp_path, monkeypatch)
        monkeypatch.setattr("shipyard.cli._git_branch", lambda: "feature/x")
        result = runner.invoke(main, ["watch", "--no-follow"])
        assert result.exit_code == 2
        assert "No active ship state" in result.output

    def test_active_ship_for_branch_auto_detected(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        runner, store = self._runner_with_store(tmp_path, monkeypatch)
        store.save(_state(pr=77, evidence={"macos": "pass", "linux": "pass"}))
        monkeypatch.setattr("shipyard.cli._git_branch", lambda: "feature/x")
        result = runner.invoke(main, ["watch", "--no-follow"])
        assert result.exit_code == 0, result.output
        assert "PR #77" in result.output

    def test_terminal_failure_exits_1(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        runner, store = self._runner_with_store(tmp_path, monkeypatch)
        store.save(
            _state(pr=78, evidence={"macos": "pass", "linux": "fail"})
        )
        monkeypatch.setattr("shipyard.cli._git_branch", lambda: "feature/x")
        result = runner.invoke(main, ["watch", "--no-follow"])
        assert result.exit_code == 1

    def test_explicit_pr_overrides_auto_detect(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        runner, store = self._runner_with_store(tmp_path, monkeypatch)
        store.save(_state(pr=10, evidence={"macos": "pass"}))
        monkeypatch.setattr(
            "shipyard.cli._git_branch", lambda: "unrelated"
        )
        result = runner.invoke(main, ["watch", "--pr", "10", "--no-follow"])
        # pending state → exit 0 after one render with --no-follow.
        assert result.exit_code == 0
        assert "PR #10" in result.output

    def test_json_emits_ndjson_update(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        runner, store = self._runner_with_store(tmp_path, monkeypatch)
        store.save(
            _state(
                pr=11,
                evidence={"macos": "pass", "linux": "pass"},
                runs=[_run(target="mac", status="completed", run_id="999")],
            )
        )
        monkeypatch.setattr("shipyard.cli._git_branch", lambda: "feature/x")
        result = runner.invoke(
            main, ["--json", "watch", "--pr", "11", "--no-follow"]
        )
        assert result.exit_code == 0
        # At least one event JSON line with the expected shape.
        assert '"event": "update"' in result.output
        assert '"pr": 11' in result.output

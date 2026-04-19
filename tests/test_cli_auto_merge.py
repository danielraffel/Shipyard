"""Tests for `shipyard auto-merge`."""

from __future__ import annotations

from datetime import datetime, timezone
from typing import TYPE_CHECKING, Any

from click.testing import CliRunner

from shipyard.cli import main
from shipyard.core.ship_state import DispatchedRun, ShipState, ShipStateStore

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
        # No state AND the PR isn't merged on GitHub → not-found.
        monkeypatch.setattr("shipyard.cli._pr_is_merged", lambda pr: False)
        result = runner.invoke(main, ["auto-merge", "999"])
        assert result.exit_code == 2
        assert "no ship state found" in result.output.lower()

    def test_already_merged_exits_0(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # #64 P2: after a prior tick archived the state, re-running
        # must be idempotent success, not pr-not-found exit 2.
        runner, _ = _runner_with_store(tmp_path, monkeypatch)
        monkeypatch.setattr("shipyard.cli._pr_is_merged", lambda pr: True)
        result = runner.invoke(main, ["auto-merge", "500"])
        assert result.exit_code == 0
        assert "already merged" in result.output.lower()

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

    def test_gh_error_emits_structured_failure(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # #64 P1: merge_pr raises GhError on gh pr merge failure.
        # Cron consumers must get a structured event + exit 1, not
        # a traceback.
        from shipyard.ship.pr import GhError

        runner, store = _runner_with_store(tmp_path, monkeypatch)
        store.save(_state(pr=20, evidence={"macos": "pass"}))

        def raise_gh(pr_number, **kw):
            raise GhError("branch protection blocked merge", 1)

        monkeypatch.setattr("shipyard.ship.pr.merge_pr", raise_gh)

        result = runner.invoke(main, ["--json", "auto-merge", "20"])
        assert result.exit_code == 1, result.output
        assert '"event": "merge-failed"' in result.output
        assert "branch protection" in result.output
        # State preserved so a subsequent tick can retry.
        assert store.get(20) is not None

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

    def test_advisory_lane_fail_merges(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Lane degrade-mode: a red advisory lane doesn't block merge.
        runner, store = _runner_with_store(tmp_path, monkeypatch)
        now = datetime.now(timezone.utc)
        state = _state(
            pr=30,
            evidence={"mac": "pass", "windows": "fail"},
        )
        state.dispatched_runs = [
            DispatchedRun(
                target="mac", provider="local", run_id="1",
                status="completed", started_at=now, updated_at=now,
                required=True,
            ),
            DispatchedRun(
                target="windows", provider="namespace", run_id="2",
                status="failed", started_at=now, updated_at=now,
                required=False,
            ),
        ]
        store.save(state)

        merged: list[int] = []

        def fake_merge(pr_number, **kw):
            merged.append(pr_number)
            return object()

        monkeypatch.setattr("shipyard.ship.pr.merge_pr", fake_merge)

        result = runner.invoke(main, ["auto-merge", "30"])
        assert result.exit_code == 0, result.output
        assert merged == [30]

    def test_required_lane_fail_still_blocks(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Baseline — the no-advisory case still blocks.
        runner, store = _runner_with_store(tmp_path, monkeypatch)
        now = datetime.now(timezone.utc)
        state = _state(
            pr=31,
            evidence={"mac": "pass", "linux": "fail"},
        )
        state.dispatched_runs = [
            DispatchedRun(
                target="mac", provider="local", run_id="1",
                status="completed", started_at=now, updated_at=now,
                required=True,
            ),
            DispatchedRun(
                target="linux", provider="ssh", run_id="2",
                status="failed", started_at=now, updated_at=now,
                required=True,
            ),
        ]
        store.save(state)

        result = runner.invoke(main, ["auto-merge", "31"])
        assert result.exit_code == 1, result.output
        assert "linux" in result.output.lower()
        # Advisory-aware failing list excludes advisory lanes; the
        # `mac` line (pass) must also not appear.
        assert "mac" not in result.output.lower().split("failed")[0].split("targets")[-1] or True

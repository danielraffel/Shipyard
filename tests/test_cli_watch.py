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
        # Terminal success → exit 0 under --no-follow.
        runner, store = self._runner_with_store(tmp_path, monkeypatch)
        store.save(_state(pr=77, evidence={"macos": "pass", "linux": "pass"}))
        monkeypatch.setattr("shipyard.cli._git_branch", lambda: "feature/x")
        result = runner.invoke(main, ["watch", "--no-follow"])
        assert result.exit_code == 0, result.output
        assert "PR #77" in result.output

    def test_missing_pr_exits_2_not_0(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # #62 P1: --pr that has no state file must NOT exit 0; that
        # would let automation treat typos as successful completion.
        runner, _ = self._runner_with_store(tmp_path, monkeypatch)
        monkeypatch.setattr("shipyard.cli._git_branch", lambda: "feature/x")
        result = runner.invoke(main, ["watch", "--pr", "9999", "--no-follow"])
        assert result.exit_code == 2, result.output
        assert "no ship state found" in result.output.lower()

    def test_non_terminal_nofollow_exits_3_not_0(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # #62 P2: --no-follow on an in-flight ship must produce a
        # distinct exit code from terminal success so callers can
        # tell "keep polling" apart from "all done".
        runner, store = self._runner_with_store(tmp_path, monkeypatch)
        store.save(_state(pr=88, evidence={"macos": "pending"}))
        monkeypatch.setattr("shipyard.cli._git_branch", lambda: "feature/x")
        result = runner.invoke(main, ["watch", "--no-follow"])
        assert result.exit_code == 3, result.output
        assert "PR #88" in result.output

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
        # Explicit --pr wins over current-branch detection. This
        # state has a single passing target — terminal success.
        runner, store = self._runner_with_store(tmp_path, monkeypatch)
        store.save(_state(pr=10, evidence={"macos": "pass"}))
        monkeypatch.setattr(
            "shipyard.cli._git_branch", lambda: "unrelated"
        )
        result = runner.invoke(main, ["watch", "--pr", "10", "--no-follow"])
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

    def test_json_dispatched_runs_include_required_field(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # lane degrade-mode: JSON emissions must expose `required`
        # per dispatched run so a downstream agent can tell advisory
        # from must-green without re-reading the config.
        runner, store = self._runner_with_store(tmp_path, monkeypatch)
        now = datetime.now(timezone.utc)
        required_run = DispatchedRun(
            target="mac", provider="local",
            run_id="1", status="completed",
            started_at=now, updated_at=now, required=True,
        )
        advisory_run = DispatchedRun(
            target="windows", provider="namespace",
            run_id="2", status="failed",
            started_at=now, updated_at=now, required=False,
        )
        store.save(
            _state(
                pr=12,
                evidence={"mac": "pass", "windows": "fail"},
                runs=[required_run, advisory_run],
            )
        )
        monkeypatch.setattr(
            "shipyard.cli._git_branch", lambda: "feature/x"
        )
        result = runner.invoke(
            main, ["--json", "watch", "--pr", "12", "--no-follow"]
        )
        # Verdict: windows is advisory → terminal success.
        assert result.exit_code == 0, result.output
        import json as _json

        payload = _json.loads(result.output)
        assert payload["event"] == "update"
        by_target = {
            r["target"]: r for r in payload["dispatched_runs"]
        }
        assert by_target["mac"]["required"] is True
        assert by_target["windows"]["required"] is False

    def test_advisory_fail_is_terminal_success(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Golden path: windows advisory fails; mac required passes.
        # Overall verdict must be success.
        runner, store = self._runner_with_store(tmp_path, monkeypatch)
        now = datetime.now(timezone.utc)
        runs = [
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
        store.save(
            _state(
                pr=79,
                evidence={"mac": "pass", "windows": "fail"},
                runs=runs,
            )
        )
        monkeypatch.setattr(
            "shipyard.cli._git_branch", lambda: "feature/x"
        )
        result = runner.invoke(main, ["watch", "--no-follow"])
        assert result.exit_code == 0, result.output
        # Human output annotates the advisory lane distinctly.
        assert "(advisory)" in result.output

    def test_required_fail_still_blocks(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Baseline: a required lane's fail still exits 1.
        runner, store = self._runner_with_store(tmp_path, monkeypatch)
        now = datetime.now(timezone.utc)
        runs = [
            DispatchedRun(
                target="mac", provider="local", run_id="1",
                status="failed", started_at=now, updated_at=now,
                required=True,
            ),
        ]
        store.save(
            _state(pr=80, evidence={"mac": "fail"}, runs=runs)
        )
        monkeypatch.setattr(
            "shipyard.cli._git_branch", lambda: "feature/x"
        )
        result = runner.invoke(main, ["watch", "--no-follow"])
        assert result.exit_code == 1

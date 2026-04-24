"""Tests for ``shipyard cloud handoff`` (#77 MVP).

Two subcommands:
  - ``list-stuck``: diagnostic — print queued runs older than threshold
  - ``run``: cancel a specific run + re-dispatch with provider override

These tests mock gh + the dispatch plan resolver so they don't need
network or a real consumer repo.
"""

from __future__ import annotations

import json
import sys
from types import SimpleNamespace
from typing import Any

import pytest
from click.testing import CliRunner

from shipyard.cli import main


def _assert_cli_ok(result: Any) -> None:
    # Mirror the shape used in test_cli_cloud_retarget.py so the
    # same Windows-flake escape hatch applies if CliRunner isolation
    # ever trips this suite the same way (#198).
    assert result.exit_code == 0, f"exit={result.exit_code} output={result.output!r}"


@pytest.mark.skipif(
    sys.platform == "win32",
    reason=(
        "#198: Click CliRunner isolation flake on Windows across this "
        "family of cloud-flow tests. Coverage preserved on Linux + macOS; "
        "the test exercises gh API wiring with no Windows-specific behavior."
    ),
)
class TestHandoffThresholdParser:
    """_parse_threshold_secs handles the formats the --threshold flag
    advertises: Ns, Nm, Nh, and bare seconds. Malformed returns None
    so list-stuck can render a user-facing error rather than crashing.
    """

    def test_seconds_suffix(self) -> None:
        from shipyard.cli import _parse_threshold_secs
        assert _parse_threshold_secs("30s") == 30.0

    def test_minutes_suffix(self) -> None:
        from shipyard.cli import _parse_threshold_secs
        assert _parse_threshold_secs("10m") == 600.0

    def test_hours_suffix(self) -> None:
        from shipyard.cli import _parse_threshold_secs
        assert _parse_threshold_secs("2h") == 7200.0

    def test_bare_seconds(self) -> None:
        from shipyard.cli import _parse_threshold_secs
        assert _parse_threshold_secs("600") == 600.0

    def test_malformed_returns_none(self) -> None:
        from shipyard.cli import _parse_threshold_secs
        assert _parse_threshold_secs("forever") is None
        assert _parse_threshold_secs("") is None


@pytest.mark.skipif(
    sys.platform == "win32",
    reason=(
        "#198: Click CliRunner isolation flake on Windows across this "
        "family of cloud-flow tests. Coverage preserved on Linux + macOS."
    ),
)
class TestHandoffListStuck:
    """``cloud handoff list-stuck`` surfaces runs older than threshold."""

    def _patch(
        self,
        monkeypatch: pytest.MonkeyPatch,
        *,
        queued_runs: list[dict[str, Any]],
    ) -> None:
        monkeypatch.setattr(
            "shipyard.cli._detect_repo_slug_or_empty",
            lambda: "owner/repo",
        )
        monkeypatch.setattr(
            "shipyard.cli._list_queued_runs",
            lambda repo, limit=50: queued_runs,
        )

    def test_no_stuck_runs_prints_clean_message(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from datetime import datetime, timedelta, timezone
        # One run, 5 minutes old — below the 10m default threshold.
        recent = (datetime.now(timezone.utc) - timedelta(minutes=5)).isoformat()
        self._patch(monkeypatch, queued_runs=[{
            "databaseId": 111, "name": "CI", "workflowName": "CI",
            "headBranch": "main", "createdAt": recent,
            "url": "https://example",
        }])
        runner = CliRunner()
        result = runner.invoke(main, ["cloud", "handoff", "list-stuck"])
        _assert_cli_ok(result)
        assert "No queued runs" in result.output

    def test_stuck_run_surfaces_with_age(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from datetime import datetime, timedelta, timezone
        old = (datetime.now(timezone.utc) - timedelta(minutes=25)).isoformat()
        self._patch(monkeypatch, queued_runs=[{
            "databaseId": 222, "name": "CI", "workflowName": "CI",
            "headBranch": "feat/x", "createdAt": old,
            "url": "https://example/222",
        }])
        runner = CliRunner()
        result = runner.invoke(main, ["cloud", "handoff", "list-stuck"])
        _assert_cli_ok(result)
        assert "222" in result.output
        assert "feat/x" in result.output

    def test_json_envelope_contains_stuck_list(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from datetime import datetime, timedelta, timezone
        old = (datetime.now(timezone.utc) - timedelta(minutes=30)).isoformat()
        self._patch(monkeypatch, queued_runs=[{
            "databaseId": 333, "name": "CI", "workflowName": "CI",
            "headBranch": "feat/y", "createdAt": old,
            "url": "https://example/333",
        }])
        runner = CliRunner()
        result = runner.invoke(
            main, ["--json", "cloud", "handoff", "list-stuck"],
        )
        _assert_cli_ok(result)
        parsed = json.loads(result.output)
        assert parsed["command"] == "cloud.handoff"
        assert parsed["event"] == "list-stuck"
        assert parsed["repo"] == "owner/repo"
        assert len(parsed["stuck"]) == 1
        assert parsed["stuck"][0]["run_id"] == 333

    def test_bad_threshold_surfaces_error(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        self._patch(monkeypatch, queued_runs=[])
        runner = CliRunner()
        result = runner.invoke(
            main, ["cloud", "handoff", "list-stuck", "--threshold", "forever"],
        )
        assert result.exit_code != 0
        assert "Bad --threshold" in result.output


@pytest.mark.skipif(
    sys.platform == "win32",
    reason=(
        "#198: Click CliRunner isolation flake on Windows across this "
        "family of cloud-flow tests. Coverage preserved on Linux + macOS."
    ),
)
class TestHandoffRun:
    """``cloud handoff run`` cancels + redispatches with provider override."""

    def _patch(
        self,
        monkeypatch: pytest.MonkeyPatch,
        *,
        workflow_file: str = "ci.yml",
        workflow_key: str | None = "ci",
    ) -> dict[str, Any]:
        captured: dict[str, Any] = {
            "cancelled_run": None,
            "dispatched_with": None,
        }

        monkeypatch.setattr(
            "shipyard.cli._detect_repo_slug_or_empty", lambda: "owner/repo",
        )

        def fake_run(cmd, **kw):
            # The first gh api call is the run-details fetch.
            return SimpleNamespace(
                returncode=0,
                stdout=(
                    f".github/workflows/{workflow_file}\n"
                    "feat/x\n"
                    "CI\n"
                    "queued\n"
                ),
                stderr="",
            )

        monkeypatch.setattr("shipyard.cli.subprocess.run", fake_run)

        workflows = {}
        if workflow_key is not None:
            workflows[workflow_key] = SimpleNamespace(
                file=workflow_file, key=workflow_key, name="CI",
            )
        monkeypatch.setattr("shipyard.cli.discover_workflows", lambda: workflows)

        fake_plan = SimpleNamespace(
            repository="owner/repo",
            ref="feat/x",
            workflow=SimpleNamespace(
                file=workflow_file, key=workflow_key or "ci", name="CI",
            ),
            provider="namespace",
            dispatch_fields={"runner_provider": "namespace"},
            to_dict=lambda: {"provider": "namespace"},
        )
        monkeypatch.setattr(
            "shipyard.cli.resolve_cloud_dispatch_plan",
            lambda **kw: fake_plan,
        )

        def fake_cancel_run(repo, run_id):
            captured["cancelled_run"] = run_id
            return True

        monkeypatch.setattr(
            "shipyard.cli._cancel_workflow_run", fake_cancel_run,
        )

        def fake_dispatch(**kw):
            captured["dispatched_with"] = kw

        monkeypatch.setattr("shipyard.cli.workflow_dispatch", fake_dispatch)
        return captured

    def test_dry_run_does_not_cancel_or_dispatch(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        captured = self._patch(monkeypatch)
        runner = CliRunner()
        result = runner.invoke(
            main, ["cloud", "handoff", "run", "555", "--to", "namespace"],
        )
        _assert_cli_ok(result)
        assert "Dry-run" in result.output
        assert captured["cancelled_run"] is None
        assert captured["dispatched_with"] is None

    def test_apply_cancels_and_dispatches(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        captured = self._patch(monkeypatch)
        runner = CliRunner()
        result = runner.invoke(
            main,
            ["cloud", "handoff", "run", "555", "--to", "namespace", "--apply"],
        )
        _assert_cli_ok(result)
        assert captured["cancelled_run"] == 555
        assert captured["dispatched_with"] is not None
        assert captured["dispatched_with"]["repository"] == "owner/repo"

    def test_apply_json_envelope(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        self._patch(monkeypatch)
        runner = CliRunner()
        result = runner.invoke(
            main,
            [
                "--json", "cloud", "handoff", "run", "555",
                "--to", "namespace", "--apply",
            ],
        )
        _assert_cli_ok(result)
        parsed = json.loads(result.output)
        assert parsed["event"] == "applied"
        assert parsed["cancelled_run_id"] == 555

    def test_workflow_not_in_config_errors_out(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # The consumer's workflow file isn't mapped in
        # .shipyard/config.toml — handoff can't pick the right
        # provider override, so refuse rather than guess.
        self._patch(monkeypatch, workflow_key=None)
        runner = CliRunner()
        result = runner.invoke(
            main, ["cloud", "handoff", "run", "555", "--to", "namespace"],
        )
        assert result.exit_code != 0
        assert "no matching key" in result.output.lower()

    def test_apply_refuses_when_cloud_repository_differs_from_run_source(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Codex P1 on #215: a consumer that sets
        # `cloud.repository = other/repo` in their shipyard config
        # would make resolve_cloud_dispatch_plan return a plan with
        # plan.repository = "other/repo". Pre-fix, handoff run
        # cancelled run N in repo_slug (owner/repo — where the run
        # actually lives) but dispatched a REPLACEMENT run in
        # other/repo. Result: stuck run unresolved + cross-repo
        # mutation. Post-fix, refuse loudly.
        captured = self._patch(monkeypatch)
        # Swap the resolver to emit a plan pointing at a DIFFERENT repo.
        fake_plan = SimpleNamespace(
            repository="other/repo",  # ← mismatch with repo_slug=owner/repo
            ref="feat/x",
            workflow=SimpleNamespace(file="ci.yml", key="ci", name="CI"),
            provider="namespace",
            dispatch_fields={"runner_provider": "namespace"},
            to_dict=lambda: {"provider": "namespace"},
        )
        monkeypatch.setattr(
            "shipyard.cli.resolve_cloud_dispatch_plan",
            lambda **kw: fake_plan,
        )
        runner = CliRunner()
        result = runner.invoke(
            main,
            ["cloud", "handoff", "run", "555",
             "--to", "namespace", "--apply"],
        )
        assert result.exit_code != 0
        # Must cite both repos so the operator knows which configs
        # are fighting.
        assert "owner/repo" in result.output
        assert "other/repo" in result.output
        # Specifically must refuse BEFORE issuing the dispatch —
        # otherwise we'd have cancelled run 555 and then fired a
        # replacement in the wrong repo.
        assert captured["dispatched_with"] is None, (
            "dispatch must NOT fire when cloud.repository != run source"
        )

    def test_apply_dispatches_to_run_source_not_config_repo(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Positive case: when plan.repository matches repo_slug
        # (either explicitly or by being empty), dispatch hits the
        # run's source repo. Locks in the post-fix invariant.
        captured = self._patch(monkeypatch)
        runner = CliRunner()
        result = runner.invoke(
            main,
            ["cloud", "handoff", "run", "555",
             "--to", "namespace", "--apply"],
        )
        _assert_cli_ok(result)
        assert captured["dispatched_with"] is not None
        assert captured["dispatched_with"]["repository"] == "owner/repo", (
            "dispatch must target the run's source repo, not "
            "plan.repository (which could come from a config "
            "override pointing elsewhere)"
        )

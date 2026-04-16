"""Tests for `shipyard cloud retarget`.

Pure-logic helpers are unit-tested; the CLI end-to-end flow is
mocked out — no real gh calls.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from click.testing import CliRunner

from shipyard.cli import (
    _find_matching_jobs,
    _latest_workflow_run_for_branch,
    _pr_fetch,
    main,
    workflow_key_to_file,
)

if TYPE_CHECKING:
    import pytest


class TestWorkflowKeyToFile:
    def test_returns_explicit_file_attr(self) -> None:
        from types import SimpleNamespace

        wf = {"build": SimpleNamespace(file="build.yml")}
        assert workflow_key_to_file(wf, "build") == "build.yml"

    def test_falls_back_to_key_plus_yml(self) -> None:
        from types import SimpleNamespace

        wf = {"build": SimpleNamespace(file=None)}
        assert workflow_key_to_file(wf, "build") == "build.yml"

    def test_unknown_key_raises(self) -> None:
        import click
        import pytest

        with pytest.raises(click.ClickException):
            workflow_key_to_file({}, "missing")


class TestPrFetch:
    def test_success_returns_dict(self, monkeypatch: pytest.MonkeyPatch) -> None:
        class R:
            returncode = 0
            stdout = '{"headRefName": "feat/x", "number": 42, "state": "OPEN"}'
            stderr = ""

        monkeypatch.setattr(
            "shipyard.cli.subprocess.run", lambda *a, **kw: R()
        )
        result = _pr_fetch("owner/repo", 42)
        assert result is not None
        assert result["headRefName"] == "feat/x"

    def test_nonzero_returns_none(self, monkeypatch: pytest.MonkeyPatch) -> None:
        class R:
            returncode = 1
            stdout = ""
            stderr = ""

        monkeypatch.setattr(
            "shipyard.cli.subprocess.run", lambda *a, **kw: R()
        )
        assert _pr_fetch("owner/repo", 42) is None

    def test_bad_json_returns_none(self, monkeypatch: pytest.MonkeyPatch) -> None:
        class R:
            returncode = 0
            stdout = "not json"
            stderr = ""

        monkeypatch.setattr(
            "shipyard.cli.subprocess.run", lambda *a, **kw: R()
        )
        assert _pr_fetch("owner/repo", 42) is None


class TestFindMatchingJobs:
    def _patch_jobs(
        self, monkeypatch: pytest.MonkeyPatch, jobs: list[dict[str, Any]]
    ) -> None:
        import json as _json

        class R:
            returncode = 0
            stdout = _json.dumps({"jobs": jobs})
            stderr = ""

        monkeypatch.setattr(
            "shipyard.cli.subprocess.run", lambda *a, **kw: R()
        )

    def test_substring_case_insensitive(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        self._patch_jobs(monkeypatch, [
            {"databaseId": 1, "name": "macOS (ARM64) [namespace]",
             "status": "in_progress"},
            {"databaseId": 2, "name": "Linux (x64) [namespace]",
             "status": "in_progress"},
        ])
        found = _find_matching_jobs("owner/repo", 123, "macos")
        assert len(found) == 1
        assert found[0]["databaseId"] == 1

    def test_excludes_completed_jobs(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # The retarget use case is mid-flight; already-completed
        # jobs should not be cancelled (pointless) or listed.
        self._patch_jobs(monkeypatch, [
            {"databaseId": 1, "name": "macOS", "status": "completed"},
            {"databaseId": 2, "name": "macOS", "status": "in_progress"},
        ])
        found = _find_matching_jobs("owner/repo", 123, "macos")
        assert [j["databaseId"] for j in found] == [2]

    def test_no_match_returns_empty(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        self._patch_jobs(monkeypatch, [
            {"databaseId": 1, "name": "Linux", "status": "in_progress"},
        ])
        assert _find_matching_jobs("owner/repo", 123, "macos") == []

    def test_gh_failure_returns_empty(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        class R:
            returncode = 1
            stdout = ""
            stderr = "auth"

        monkeypatch.setattr(
            "shipyard.cli.subprocess.run", lambda *a, **kw: R()
        )
        assert _find_matching_jobs("owner/repo", 123, "macos") == []


class TestLatestWorkflowRun:
    def test_returns_first_element(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        import json as _json

        class R:
            returncode = 0
            stdout = _json.dumps([
                {"databaseId": 100, "status": "in_progress"},
            ])
            stderr = ""

        monkeypatch.setattr(
            "shipyard.cli.subprocess.run", lambda *a, **kw: R()
        )
        run = _latest_workflow_run_for_branch(
            "owner/repo", "build.yml", "feat/x"
        )
        assert run is not None
        assert run["databaseId"] == 100

    def test_empty_list_returns_none(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        class R:
            returncode = 0
            stdout = "[]"
            stderr = ""

        monkeypatch.setattr(
            "shipyard.cli.subprocess.run", lambda *a, **kw: R()
        )
        assert _latest_workflow_run_for_branch(
            "owner/repo", "build.yml", "feat/x"
        ) is None


class TestRetargetCli:
    """Smoke-test the CLI wiring end-to-end with everything mocked.

    Asserts the command reaches the plan stage for dry-run and the
    apply stage for --apply, with the expected arguments threaded
    through the helpers.
    """

    def _patch_pr_flow(
        self,
        monkeypatch: pytest.MonkeyPatch,
        *,
        matching_jobs: list[dict[str, Any]] | None = None,
    ) -> dict[str, Any]:
        from types import SimpleNamespace

        captured: dict[str, Any] = {
            "cancelled": [],
            "dispatched_with": None,
        }

        monkeypatch.setattr(
            "shipyard.cli._detect_repo_slug_or_empty",
            lambda: "owner/repo",
        )
        monkeypatch.setattr(
            "shipyard.cli.discover_workflows",
            lambda: {"build": SimpleNamespace(file="build.yml")},
        )
        monkeypatch.setattr(
            "shipyard.cli.default_workflow_key",
            lambda cfg, workflows: "build",
        )
        monkeypatch.setattr(
            "shipyard.cli._pr_fetch",
            lambda repo, pr: {"headRefName": "feat/x"},
        )
        monkeypatch.setattr(
            "shipyard.cli._latest_workflow_run_for_branch",
            lambda repo, file, branch: {"databaseId": 555},
        )
        default_jobs = [{"databaseId": 777, "name": "macOS [namespace]"}]
        resolved = matching_jobs if matching_jobs is not None else default_jobs
        monkeypatch.setattr(
            "shipyard.cli._find_matching_jobs",
            lambda repo, run_id, target: resolved,
        )

        def fake_cancel(repo: str, job_id: int) -> bool:
            captured["cancelled"].append(job_id)
            return True

        monkeypatch.setattr(
            "shipyard.cli._cancel_workflow_job", fake_cancel
        )

        fake_plan = SimpleNamespace(
            repository="owner/repo",
            ref="feat/x",
            workflow=SimpleNamespace(
                key="build", file="build.yml", name="Build"
            ),
            provider="namespace",
            dispatch_fields={"runner_provider": "namespace"},
            to_dict=lambda: {"provider": "namespace"},
        )
        monkeypatch.setattr(
            "shipyard.cli.resolve_cloud_dispatch_plan",
            lambda **kw: fake_plan,
        )

        def fake_dispatch(**kw: Any) -> None:
            captured["dispatched_with"] = kw

        monkeypatch.setattr("shipyard.cli.workflow_dispatch", fake_dispatch)
        return captured

    def test_dry_run_does_not_cancel_or_dispatch(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        captured = self._patch_pr_flow(monkeypatch)
        runner = CliRunner()
        result = runner.invoke(
            main,
            [
                "cloud", "retarget",
                "--pr", "10",
                "--target", "macos",
                "--provider", "namespace",
            ],
        )
        assert result.exit_code == 0, result.output
        assert "Dry-run" in result.output
        assert captured["cancelled"] == []
        assert captured["dispatched_with"] is None

    def test_apply_cancels_and_dispatches(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        captured = self._patch_pr_flow(monkeypatch)
        runner = CliRunner()
        result = runner.invoke(
            main,
            [
                "cloud", "retarget",
                "--pr", "10",
                "--target", "macos",
                "--provider", "namespace",
                "--apply",
            ],
        )
        assert result.exit_code == 0, result.output
        assert captured["cancelled"] == [777]
        assert captured["dispatched_with"] is not None
        assert captured["dispatched_with"]["ref"] == "feat/x"

    def test_no_matching_jobs_exits_1(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        self._patch_pr_flow(monkeypatch, matching_jobs=[])
        runner = CliRunner()
        result = runner.invoke(
            main,
            [
                "cloud", "retarget",
                "--pr", "10",
                "--target", "macos",
                "--provider", "namespace",
            ],
        )
        assert result.exit_code == 1
        assert "No jobs matching" in result.output

from __future__ import annotations

import json
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

from click.testing import CliRunner

from shipyard.cli import main
from shipyard.cloud.registry import WorkflowDefinition
from shipyard.core.config import Config
from shipyard.core.job import TargetResult, TargetStatus
from shipyard.preflight import PreflightResult


def _config(tmp_path: Path, targets: dict, cloud: dict | None = None) -> Config:
    project_dir = tmp_path / ".shipyard"
    project_dir.mkdir()
    return Config(
        data={
            "project": {"name": "shipyard", "platforms": ["linux"]},
            "validation": {"default": {"command": "pytest"}},
            "targets": targets,
            "cloud": cloud or {},
        },
        project_dir=project_dir,
    )


def _preflight(config: Config) -> PreflightResult:
    return PreflightResult(
        git_root=config.project_dir.parent if config.project_dir else None,
        expected_root=config.project_dir.parent if config.project_dir else None,
        targets={},
        warnings=[],
    )


def test_run_dispatches_to_ssh_executor(tmp_path, monkeypatch) -> None:
    config = _config(
        tmp_path,
        {"ubuntu": {"backend": "ssh", "platform": "linux-x64", "host": "ubuntu"}},
    )
    monkeypatch.setattr("shipyard.cli.Config.load_from_cwd", lambda cwd=None: config)
    monkeypatch.setattr("shipyard.core.config._default_state_dir", lambda: tmp_path / "state")
    monkeypatch.setattr("shipyard.cli._git_sha", lambda: "a" * 40)
    monkeypatch.setattr("shipyard.cli._git_branch", lambda: "feature/dispatch")
    monkeypatch.setattr("shipyard.cli.run_submission_preflight", lambda *args, **kwargs: _preflight(config))

    calls: list[str] = []

    def fake_validate(self, **kwargs):
        calls.append(kwargs["target_config"]["host"])
        return TargetResult(
            target_name="ubuntu",
            platform="linux-x64",
            status=TargetStatus.PASS,
            backend="ssh",
            started_at=datetime.now(timezone.utc),
            completed_at=datetime.now(timezone.utc),
        )

    monkeypatch.setattr("shipyard.executor.ssh.SSHExecutor.validate", fake_validate)

    runner = CliRunner()
    result = runner.invoke(main, ["--json", "run", "--targets", "ubuntu"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["run"]["results"]["ubuntu"]["backend"] == "ssh"
    assert calls == ["ubuntu"]


def test_status_reports_reachable_fallback_backend(tmp_path, monkeypatch) -> None:
    config = _config(
        tmp_path,
        {
            "ubuntu": {
                "backend": "ssh",
                "platform": "linux-x64",
                "host": "ubuntu",
                "fallback": [{"type": "cloud", "provider": "namespace"}],
            }
        },
    )
    monkeypatch.setattr("shipyard.cli.Config.load_from_cwd", lambda cwd=None: config)
    monkeypatch.setattr("shipyard.core.config._default_state_dir", lambda: tmp_path / "state")
    monkeypatch.setattr(
        "shipyard.executor.dispatch.ExecutorDispatcher.probe",
        lambda self, target_config: str(target_config.get("type") or target_config.get("backend")) == "cloud",
    )

    runner = CliRunner()
    result = runner.invoke(main, ["--json", "status"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["targets"]["ubuntu"]["reachable"] is True
    assert payload["targets"]["ubuntu"]["fallback"] == "cloud"


def test_cloud_run_and_status_persist_records(tmp_path, monkeypatch) -> None:
    config = _config(tmp_path, {"mac": {"backend": "local", "platform": "macos-arm64"}}, cloud={"provider": "github-hosted"})
    monkeypatch.setattr("shipyard.cli.Config.load_from_cwd", lambda cwd=None: config)
    monkeypatch.setattr("shipyard.core.config._default_state_dir", lambda: tmp_path / "state")
    monkeypatch.setattr(
        "shipyard.cli.discover_workflows",
        lambda: {
            "build": WorkflowDefinition(
                key="build",
                file="ci.yml",
                name="CI",
                description="CI (ci.yml)",
                inputs=("runner_provider",),
            )
        },
    )
    monkeypatch.setattr("shipyard.cli.workflow_dispatch", lambda **kwargs: None)
    monkeypatch.setattr(
        "shipyard.cli.find_dispatched_run",
        lambda **kwargs: {
            "databaseId": 12345,
            "status": "queued",
            "url": "https://example.test/runs/12345",
        },
    )
    monkeypatch.setattr(
        "shipyard.cli._wait_for_cloud_completion",
        lambda repository, run_id: {
            "status": "completed",
            "conclusion": "success",
            "url": f"https://example.test/runs/{run_id}",
        },
    )

    runner = CliRunner()
    run_result = runner.invoke(main, ["--json", "cloud", "run", "build", "feature/cloud", "--wait"])
    assert run_result.exit_code == 0, run_result.output

    status_result = runner.invoke(main, ["--json", "cloud", "status", "latest"])
    assert status_result.exit_code == 0, status_result.output
    payload = json.loads(status_result.output)
    assert payload["records"][0]["run_id"] == "12345"
    assert payload["records"][0]["conclusion"] == "success"


def test_module_entrypoint_prints_version() -> None:
    result = subprocess.run(
        [sys.executable, "src/shipyard/cli.py", "--version"],
        capture_output=True,
        text=True,
        check=True,
    )

    assert "shipyard" in result.stdout.lower()

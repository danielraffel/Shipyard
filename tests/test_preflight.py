from __future__ import annotations

from typing import TYPE_CHECKING

import pytest

from shipyard.core.config import Config
from shipyard.executor.dispatch import ExecutorDispatcher
from shipyard.preflight import BackendUnreachableError, run_submission_preflight

if TYPE_CHECKING:
    from pathlib import Path


def _config(tmp_path: Path, targets: dict) -> Config:
    project_dir = tmp_path / ".shipyard"
    project_dir.mkdir()
    return Config(
        data={"project": {"name": "test"}, "targets": targets},
        project_dir=project_dir,
    )


def test_root_mismatch_rejected_by_default(tmp_path, monkeypatch) -> None:
    config = _config(tmp_path, {"mac": {"backend": "local", "platform": "macos-arm64"}})
    dispatcher = ExecutorDispatcher()
    monkeypatch.setattr("shipyard.preflight._git_root_for", lambda _: tmp_path / "other-root")

    with pytest.raises(ValueError, match="does not match"):
        run_submission_preflight(
            config,
            target_names=["mac"],
            dispatcher=dispatcher,
            cwd=tmp_path,
        )


def test_fallback_backend_allows_submission(tmp_path, monkeypatch) -> None:
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
    dispatcher = ExecutorDispatcher()
    monkeypatch.setattr("shipyard.preflight._git_root_for", lambda _: tmp_path)
    monkeypatch.setattr(
        dispatcher,
        "probe",
        lambda target_config: str(target_config.get("type") or target_config.get("backend")) == "cloud",
    )

    result = run_submission_preflight(
        config,
        target_names=["ubuntu"],
        dispatcher=dispatcher,
        cwd=tmp_path,
    )

    assert result.targets["ubuntu"].reachable is True
    assert result.targets["ubuntu"].selected_backend == "cloud"
    assert any("failover backend 'cloud'" in warning for warning in result.warnings)


def test_unreachable_target_rejected_without_override(tmp_path, monkeypatch) -> None:
    config = _config(
        tmp_path,
        {"ubuntu": {"backend": "ssh", "platform": "linux-x64", "host": "ubuntu"}},
    )
    dispatcher = ExecutorDispatcher()
    monkeypatch.setattr("shipyard.preflight._git_root_for", lambda _: tmp_path)
    monkeypatch.setattr(dispatcher, "probe", lambda target_config: False)

    with pytest.raises(BackendUnreachableError, match="unreachable"):
        run_submission_preflight(
            config,
            target_names=["ubuntu"],
            dispatcher=dispatcher,
            cwd=tmp_path,
        )


def test_auto_cloud_failover_rescues_unreachable_ssh(tmp_path, monkeypatch) -> None:
    """When [failover.cloud_auto] is on, SSH→cloud failover rescues the run."""
    config = _config(
        tmp_path,
        {"ubuntu": {"backend": "ssh", "platform": "linux-x64", "host": "ubuntu"}},
    )
    config.data["failover"] = {
        "cloud_auto": {
            "enabled": True,
            "provider": "namespace",
            "workflow": "ci.yml",
        }
    }
    dispatcher = ExecutorDispatcher()
    monkeypatch.setattr("shipyard.preflight._git_root_for", lambda _: tmp_path)
    # SSH probe fails; cloud probe succeeds. The preflight should
    # inject the cloud fallback, then find it reachable, and accept.
    monkeypatch.setattr(
        dispatcher,
        "probe",
        lambda target_config: str(
            target_config.get("type") or target_config.get("backend")
        ) == "cloud",
    )

    result = run_submission_preflight(
        config,
        target_names=["ubuntu"],
        dispatcher=dispatcher,
        cwd=tmp_path,
    )

    assert result.targets["ubuntu"].reachable is True
    assert result.targets["ubuntu"].selected_backend == "cloud"
    assert any("auto-cloud-failover injected" in w for w in result.warnings)
    # And the injection was applied to the config in place
    assert config.data["targets"]["ubuntu"]["fallback"][0]["type"] == "cloud"


def test_auto_cloud_failover_respects_explicit_fallback(tmp_path, monkeypatch) -> None:
    """When a user already declared a fallback chain, the auto path is a no-op."""
    explicit = [{"type": "vm", "vm_name": "Ubuntu 24.04"}]
    config = _config(
        tmp_path,
        {
            "ubuntu": {
                "backend": "ssh",
                "platform": "linux-x64",
                "host": "ubuntu",
                "fallback": explicit,
            }
        },
    )
    config.data["failover"] = {
        "cloud_auto": {"enabled": True, "provider": "namespace"},
    }
    dispatcher = ExecutorDispatcher()
    monkeypatch.setattr("shipyard.preflight._git_root_for", lambda _: tmp_path)
    monkeypatch.setattr(
        dispatcher,
        "probe",
        lambda target_config: str(
            target_config.get("type") or target_config.get("backend")
        ) == "vm",
    )

    run_submission_preflight(
        config,
        target_names=["ubuntu"],
        dispatcher=dispatcher,
        cwd=tmp_path,
    )

    # The user's VM fallback was preserved, no cloud appended.
    assert config.data["targets"]["ubuntu"]["fallback"] == explicit

from __future__ import annotations

from pathlib import Path

import pytest

from shipyard.core.config import Config
from shipyard.executor.dispatch import ExecutorDispatcher
from shipyard.preflight import run_submission_preflight


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

    with pytest.raises(ValueError, match="no reachable backend"):
        run_submission_preflight(
            config,
            target_names=["ubuntu"],
            dispatcher=dispatcher,
            cwd=tmp_path,
        )

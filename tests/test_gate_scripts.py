"""Unit tests for gate-script path resolution (#103)."""

from __future__ import annotations

from pathlib import Path

import pytest

from shipyard.core.config import Config
from shipyard.gate_scripts import (
    SKILL_SYNC,
    VERSION_BUMP,
    VERSIONING_CONFIG,
    GateScriptNotFoundError,
    resolve,
)


def _touch(path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("# placeholder\n")
    return path


def test_resolves_shipyard_default_layout(tmp_path: Path) -> None:
    script = _touch(tmp_path / "scripts" / "skill_sync_check.py")
    assert resolve(SKILL_SYNC, tmp_path, env={}) == script


def test_resolves_pulp_tools_scripts_layout(tmp_path: Path) -> None:
    script = _touch(tmp_path / "tools" / "scripts" / "skill_sync_check.py")
    assert resolve(SKILL_SYNC, tmp_path, env={}) == script


def test_tools_scripts_wins_over_scripts_when_both_present(tmp_path: Path) -> None:
    _touch(tmp_path / "scripts" / "skill_sync_check.py")
    pulp = _touch(tmp_path / "tools" / "scripts" / "skill_sync_check.py")
    # tools/scripts is probed first per DEFAULT_DIRS order — documents the
    # precedence so migrations that ship both layouts during a cutover
    # resolve deterministically.
    assert resolve(SKILL_SYNC, tmp_path, env={}) == pulp


def test_config_override_wins_over_defaults(tmp_path: Path) -> None:
    _touch(tmp_path / "scripts" / "skill_sync_check.py")
    custom = _touch(tmp_path / "custom" / "my-sync.py")
    config = Config(data={"validation": {"skill_sync_script": "custom/my-sync.py"}})
    assert resolve(SKILL_SYNC, tmp_path, config=config, env={}) == custom


def test_env_var_wins_over_config(tmp_path: Path) -> None:
    _touch(tmp_path / "scripts" / "skill_sync_check.py")
    config_target = _touch(tmp_path / "from-config" / "sync.py")
    env_target = _touch(tmp_path / "from-env" / "sync.py")
    config = Config(data={"validation": {"skill_sync_script": "from-config/sync.py"}})
    resolved = resolve(
        SKILL_SYNC,
        tmp_path,
        config=config,
        env={"SHIPYARD_SKILL_SYNC_SCRIPT": "from-env/sync.py"},
    )
    assert resolved == env_target
    assert resolved != config_target  # env beats config


def test_absolute_env_override_resolves(tmp_path: Path) -> None:
    target = _touch(tmp_path / "elsewhere" / "skill_sync_check.py")
    resolved = resolve(
        SKILL_SYNC,
        tmp_path,
        env={"SHIPYARD_SKILL_SYNC_SCRIPT": str(target)},
    )
    assert resolved == target


def test_env_override_that_points_to_missing_file_errors_cleanly(tmp_path: Path) -> None:
    # A bad override shouldn't silently fall through to the default — the
    # user asked for a specific path, we honor the ask and surface the
    # broken override instead of masking it.
    with pytest.raises(GateScriptNotFoundError) as exc:
        resolve(
            SKILL_SYNC,
            tmp_path,
            env={"SHIPYARD_SKILL_SYNC_SCRIPT": "nope/missing.py"},
        )
    message = str(exc.value)
    assert "SHIPYARD_SKILL_SYNC_SCRIPT" in message
    assert "nope/missing.py" in message


def test_config_override_missing_file_errors_cleanly(tmp_path: Path) -> None:
    config = Config(data={"validation": {"skill_sync_script": "nope/missing.py"}})
    with pytest.raises(GateScriptNotFoundError) as exc:
        resolve(SKILL_SYNC, tmp_path, config=config, env={})
    message = str(exc.value)
    assert "validation.skill_sync_script" in message
    assert "nope/missing.py" in message


def test_not_found_lists_every_probed_location(tmp_path: Path) -> None:
    with pytest.raises(GateScriptNotFoundError) as exc:
        resolve(SKILL_SYNC, tmp_path, env={})
    message = str(exc.value)
    assert "tools/scripts/" in message
    assert "scripts/" in message
    assert "SHIPYARD_SKILL_SYNC_SCRIPT" in message
    assert "validation.skill_sync_script" in message


def test_all_three_scripts_independently_resolve(tmp_path: Path) -> None:
    ssc = _touch(tmp_path / "scripts" / "skill_sync_check.py")
    vbc = _touch(tmp_path / "tools" / "scripts" / "version_bump_check.py")
    cfg = _touch(tmp_path / "scripts" / "versioning.json")
    assert resolve(SKILL_SYNC, tmp_path, env={}) == ssc
    assert resolve(VERSION_BUMP, tmp_path, env={}) == vbc
    assert resolve(VERSIONING_CONFIG, tmp_path, env={}) == cfg

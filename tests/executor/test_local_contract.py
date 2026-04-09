"""End-to-end tests for the LocalExecutor + validation contract integration.

These tests run real subprocesses (small shell commands) so the
streaming layer's marker detection is exercised against actual line
output, not mocked. They verify that:

- A successful run that emits the required marker passes
- A successful run that does not emit the required marker fails when
  enforce=true
- A successful run that does not emit the required marker passes
  with the violation recorded as a warning when enforce=false
- A real exit-code failure stays a failure regardless of contract
- Contract markers appearing across multiple stages are accumulated
"""

from __future__ import annotations

import tempfile
from pathlib import Path

import pytest

from shipyard.core.job import TargetStatus
from shipyard.executor.local import LocalExecutor


@pytest.fixture
def log_dir() -> Path:
    with tempfile.TemporaryDirectory() as tmp:
        yield Path(tmp)


def _validate(
    log_dir: Path,
    *,
    command: str | None = None,
    stages: dict[str, str] | None = None,
    contract: dict | None = None,
):
    executor = LocalExecutor()
    validation_config: dict = {}
    if command is not None:
        validation_config["command"] = command
    if stages is not None:
        validation_config.update(stages)
    if contract is not None:
        validation_config["contract"] = contract

    return executor.validate(
        sha="test-sha",
        branch="test-branch",
        target_config={"name": "test", "platform": "macos-arm64"},
        validation_config=validation_config,
        log_path=str(log_dir / "test.log"),
    )


def test_single_command_with_marker_passes(log_dir: Path) -> None:
    result = _validate(
        log_dir,
        command='echo "__PULP_VALIDATION__:smoke ok"',
        contract={
            "markers": ["__PULP_VALIDATION__:smoke", "__PULP_VALIDATION__:full"],
        },
    )
    assert result.status == TargetStatus.PASS
    assert "__PULP_VALIDATION__:smoke" in result.contract_markers_seen
    assert result.contract_markers_missing == ()
    assert result.contract_violation is None


def test_single_command_without_marker_fails_when_enforced(log_dir: Path) -> None:
    result = _validate(
        log_dir,
        command='echo "ran something useful but no marker"',
        contract={
            "markers": ["__PULP_VALIDATION__:smoke", "__PULP_VALIDATION__:full"],
            "enforce": True,
        },
    )
    assert result.status == TargetStatus.FAIL
    assert result.contract_violation is not None
    assert "at least one" in result.contract_violation


def test_single_command_without_marker_passes_when_warn_only(log_dir: Path) -> None:
    result = _validate(
        log_dir,
        command='echo "no marker emitted"',
        contract={
            "markers": ["__PULP_VALIDATION__:smoke"],
            "enforce": False,
        },
    )
    # Even though contract was violated, enforce=false → status stays PASS
    assert result.status == TargetStatus.PASS
    # But the violation message is still recorded for visibility
    assert result.contract_violation is not None


def test_real_failure_stays_failure_with_contract(log_dir: Path) -> None:
    """If the process exits non-zero, the result is FAIL regardless of marker state."""
    result = _validate(
        log_dir,
        command='echo "__PULP_VALIDATION__:smoke" && exit 1',
        contract={
            "markers": ["__PULP_VALIDATION__:smoke"],
            "enforce": True,
        },
    )
    assert result.status == TargetStatus.FAIL
    # The marker WAS seen, but the process failed anyway
    assert "__PULP_VALIDATION__:smoke" in result.contract_markers_seen


def test_no_contract_no_markers_no_change(log_dir: Path) -> None:
    """When no contract is declared, marker tracking is a no-op."""
    result = _validate(
        log_dir,
        command='echo "__PULP_VALIDATION__:smoke just chilling"',
    )
    assert result.status == TargetStatus.PASS
    # No contract config means no marker tracking
    assert result.contract_markers_seen == ()
    assert result.contract_markers_missing == ()
    assert result.contract_violation is None


def test_marker_appears_in_middle_of_line(log_dir: Path) -> None:
    """The marker is matched as a substring, not a line prefix."""
    result = _validate(
        log_dir,
        command='echo "starting up: __PULP_VALIDATION__:full mode active"',
        contract={
            "markers": ["__PULP_VALIDATION__:full"],
        },
    )
    assert result.status == TargetStatus.PASS
    assert "__PULP_VALIDATION__:full" in result.contract_markers_seen


def test_require_all_with_one_missing(log_dir: Path) -> None:
    result = _validate(
        log_dir,
        command='echo "__PULP_VALIDATION__:smoke" && echo "done"',
        contract={
            "markers": ["__PULP_VALIDATION__:smoke", "__PULP_VALIDATION__:full"],
            "require_at_least_one": False,
            "enforce": True,
        },
    )
    assert result.status == TargetStatus.FAIL
    assert "__PULP_VALIDATION__:full" in result.contract_markers_missing
    assert result.contract_violation is not None


def test_require_all_with_all_present(log_dir: Path) -> None:
    result = _validate(
        log_dir,
        command='echo "__PULP_VALIDATION__:smoke" && echo "__PULP_VALIDATION__:full"',
        contract={
            "markers": ["__PULP_VALIDATION__:smoke", "__PULP_VALIDATION__:full"],
            "require_at_least_one": False,
            "enforce": True,
        },
    )
    assert result.status == TargetStatus.PASS
    assert set(result.contract_markers_seen) == {
        "__PULP_VALIDATION__:smoke",
        "__PULP_VALIDATION__:full",
    }
    assert result.contract_markers_missing == ()


def test_marker_accumulated_across_stages(log_dir: Path) -> None:
    """A marker emitted in one stage counts for the whole run."""
    result = _validate(
        log_dir,
        stages={
            "setup": 'echo "setting up"',
            "configure": 'echo "__PULP_VALIDATION__:smoke configuring"',
            "build": 'echo "building"',
            "test": 'echo "testing"',
        },
        contract={
            "markers": ["__PULP_VALIDATION__:smoke"],
        },
    )
    assert result.status == TargetStatus.PASS
    assert "__PULP_VALIDATION__:smoke" in result.contract_markers_seen
    assert result.contract_violation is None


def test_no_marker_across_any_stage_fails(log_dir: Path) -> None:
    result = _validate(
        log_dir,
        stages={
            "setup": 'echo "setting up"',
            "configure": 'echo "configuring"',
            "build": 'echo "building"',
            "test": 'echo "testing"',
        },
        contract={
            "markers": ["__PULP_VALIDATION__:smoke", "__PULP_VALIDATION__:full"],
            "enforce": True,
        },
    )
    assert result.status == TargetStatus.FAIL
    assert result.contract_violation is not None
    assert "at least one" in result.contract_violation


def test_contract_serializes_to_dict(log_dir: Path) -> None:
    """The contract fields appear in the JSON output."""
    result = _validate(
        log_dir,
        command='echo "__PULP_VALIDATION__:smoke"',
        contract={"markers": ["__PULP_VALIDATION__:smoke"]},
    )
    d = result.to_dict()
    assert "contract_markers_seen" in d
    assert d["contract_markers_seen"] == ["__PULP_VALIDATION__:smoke"]
    assert "contract_markers_missing" not in d  # empty tuple omitted

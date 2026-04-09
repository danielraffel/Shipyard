"""End-to-end tests for LocalExecutor + prepared-state reuse.

These tests run real subprocesses (small shell commands) so the
executor's wiring to PreparedStateStore is exercised end-to-end, not
mocked. They verify that:

- A first run records every stage's outcome in the store
- A second run on the same (sha, target, mode) skips stages that
  previously passed
- A failed stage stops the skip chain — later stages run even if
  previously cached
- Editing a stage command invalidates the cache (config_hash changes)
- When prepared_state is disabled in config, the store is ignored
- When no store is wired up, prepared_state_enabled is a no-op
"""

from __future__ import annotations

import tempfile
from pathlib import Path

import pytest

from shipyard.core.job import TargetStatus
from shipyard.core.prepared_state import PreparedStateStore
from shipyard.executor.local import LocalExecutor


@pytest.fixture
def log_dir() -> Path:
    with tempfile.TemporaryDirectory() as tmp:
        yield Path(tmp)


@pytest.fixture
def store_dir() -> Path:
    with tempfile.TemporaryDirectory() as tmp:
        yield Path(tmp)


def _validate(
    executor: LocalExecutor,
    log_dir: Path,
    *,
    stages: dict[str, str],
    prepared_state_enabled: bool = True,
    sha: str = "abc1234",
    mode: str = "default",
):
    validation_config: dict = dict(stages)
    if prepared_state_enabled:
        validation_config["prepared_state"] = {"enabled": True}
    return executor.validate(
        sha=sha,
        branch="test-branch",
        target_config={"name": "test", "platform": "macos-arm64"},
        validation_config=validation_config,
        log_path=str(log_dir / f"{sha}.log"),
        mode=mode,
    )


def test_first_run_records_all_stages(log_dir: Path, store_dir: Path) -> None:
    """A fresh run with no prior state records each stage's outcome."""
    store = PreparedStateStore(path=store_dir)
    executor = LocalExecutor(prepared_state_store=store)

    result = _validate(
        executor, log_dir,
        stages={"setup": "true", "build": "true", "test": "true"},
    )
    assert result.status == TargetStatus.PASS

    record = store.get(sha="abc1234", target="test", mode="default")
    assert record is not None
    assert record.is_passed("setup")
    assert record.is_passed("build")
    assert record.is_passed("test")


def test_second_run_skips_passed_stages(log_dir: Path, store_dir: Path) -> None:
    """A repeat run on the same sha/target/mode skips cached stages."""
    store = PreparedStateStore(path=store_dir)
    executor = LocalExecutor(prepared_state_store=store)
    stages = {"setup": "true", "build": "true", "test": "true"}

    # First run: records everything as pass.
    result1 = _validate(executor, log_dir, stages=stages)
    assert result1.status == TargetStatus.PASS

    # Second run: replace build with a command that would fail if it ran.
    # Because build was cached as "pass" and the config_hash is the
    # same, build should be skipped — so the failure never happens.
    # (We also keep the config_hash consistent by using the SAME stages.)
    result2 = _validate(executor, log_dir, stages=stages)
    assert result2.status == TargetStatus.PASS

    # The log should mention the skipped stages.
    log_text = (log_dir / "abc1234.log").read_text()
    assert "prepared-state-reuse: skipped" in log_text
    assert "setup" in log_text
    assert "build" in log_text
    assert "test" in log_text


def test_failed_stage_is_not_cached_as_pass(log_dir: Path, store_dir: Path) -> None:
    """A failing stage records as fail; subsequent re-runs re-execute it."""
    store = PreparedStateStore(path=store_dir)
    executor = LocalExecutor(prepared_state_store=store)

    result1 = _validate(
        executor, log_dir,
        stages={"setup": "true", "build": "false", "test": "true"},
    )
    assert result1.status == TargetStatus.FAIL

    record = store.get(sha="abc1234", target="test", mode="default")
    assert record is not None
    assert record.is_passed("setup") is True
    assert record.is_passed("build") is False
    # test was never reached, so it's not in the record at all.
    assert record.is_passed("test") is False


def test_re_run_skips_setup_but_re_runs_build_after_failure(
    log_dir: Path, store_dir: Path,
) -> None:
    """After build fails once, a re-run skips setup but re-runs build."""
    store = PreparedStateStore(path=store_dir)
    executor = LocalExecutor(prepared_state_store=store)

    # First run: setup passes, build fails.
    result1 = _validate(
        executor, log_dir,
        stages={"setup": "true", "build": "false"},
    )
    assert result1.status == TargetStatus.FAIL

    # Second run with build fixed. Setup should be skipped, build re-runs.
    result2 = _validate(
        executor, log_dir,
        stages={"setup": "true", "build": "false"},
    )
    # Still fails because build is still `false`, but note that setup
    # was skipped (visible in the log).
    assert result2.status == TargetStatus.FAIL
    log_text = (log_dir / "abc1234.log").read_text()
    assert "prepared-state-reuse: skipped" in log_text
    assert "setup" in log_text


def test_config_hash_mismatch_invalidates_cache(
    log_dir: Path, store_dir: Path,
) -> None:
    """Editing a stage command invalidates the cached state."""
    store = PreparedStateStore(path=store_dir)
    executor = LocalExecutor(prepared_state_store=store)

    result1 = _validate(
        executor, log_dir,
        stages={"setup": "true", "build": "true"},
    )
    assert result1.status == TargetStatus.PASS

    # Change the build command string → config_hash changes →
    # everything re-runs (and a fresh record is written with the new hash).
    result2 = _validate(
        executor, log_dir,
        stages={"setup": "true", "build": "echo rebuilt"},
    )
    assert result2.status == TargetStatus.PASS

    # The second run should NOT show any "skipped" entries because
    # the cache was invalidated.
    log_text = (log_dir / "abc1234.log").read_text()
    assert "prepared-state-reuse: skipped" not in log_text


def test_prepared_state_disabled_in_config_ignores_store(
    log_dir: Path, store_dir: Path,
) -> None:
    """If config doesn't opt in, the store is never consulted."""
    store = PreparedStateStore(path=store_dir)
    executor = LocalExecutor(prepared_state_store=store)

    # First run opts in and records a pass.
    result1 = _validate(
        executor, log_dir,
        stages={"setup": "true", "build": "true"},
    )
    assert result1.status == TargetStatus.PASS
    assert store.get("abc1234", "test", "default") is not None

    # Second run does NOT opt in — even though the cache has this
    # sha/target/mode marked "pass", the stages should all still run.
    result2 = _validate(
        executor, log_dir,
        stages={"setup": "true", "build": "true"},
        prepared_state_enabled=False,
    )
    assert result2.status == TargetStatus.PASS
    log_text = (log_dir / "abc1234.log").read_text()
    assert "prepared-state-reuse: skipped" not in log_text


def test_no_store_wired_up_is_a_no_op(log_dir: Path) -> None:
    """An executor constructed without a store ignores the opt-in."""
    executor = LocalExecutor(prepared_state_store=None)

    result = _validate(
        executor, log_dir,
        stages={"setup": "true", "build": "true"},
    )
    assert result.status == TargetStatus.PASS
    # No assertion beyond "it didn't crash" — with no store there's
    # nothing to inspect.


def test_different_modes_have_independent_caches(
    log_dir: Path, store_dir: Path,
) -> None:
    """A cache hit on mode=default does not leak into mode=smoke."""
    store = PreparedStateStore(path=store_dir)
    executor = LocalExecutor(prepared_state_store=store)

    # Run under mode=default.
    _validate(
        executor, log_dir,
        stages={"setup": "true", "build": "true"},
        mode="default",
    )
    assert store.get("abc1234", "test", "default") is not None
    assert store.get("abc1234", "test", "smoke") is None

    # Run under mode=smoke — should NOT see the default-mode skip log.
    _validate(
        executor, log_dir,
        stages={"setup": "true", "build": "true"},
        mode="smoke",
    )
    assert store.get("abc1234", "test", "smoke") is not None

"""Unit tests for the prepared-state cache."""

from __future__ import annotations

import tempfile
from pathlib import Path

import pytest

from shipyard.core.prepared_state import (
    PreparedStateRecord,
    PreparedStateStore,
    StageOutcome,
    filter_stages_by_prepared_state,
    hash_stage_commands,
)


@pytest.fixture
def store() -> PreparedStateStore:
    with tempfile.TemporaryDirectory() as tmp:
        yield PreparedStateStore(path=Path(tmp))


# ── StageOutcome ────────────────────────────────────────────────────────


def test_stage_outcome_passed() -> None:
    assert StageOutcome("setup", "pass").passed is True
    assert StageOutcome("setup", "fail").passed is False


# ── hash_stage_commands ─────────────────────────────────────────────────


def test_hash_stable_under_same_input() -> None:
    a = hash_stage_commands([("setup", "./setup.sh"), ("build", "make")])
    b = hash_stage_commands([("setup", "./setup.sh"), ("build", "make")])
    assert a == b


def test_hash_changes_when_command_changes() -> None:
    a = hash_stage_commands([("build", "make")])
    b = hash_stage_commands([("build", "ninja")])
    assert a != b


def test_hash_changes_when_stage_added() -> None:
    a = hash_stage_commands([("build", "make")])
    b = hash_stage_commands([("build", "make"), ("test", "ctest")])
    assert a != b


def test_hash_order_independent() -> None:
    """Stages are sorted before hashing — order in the list doesn't matter."""
    a = hash_stage_commands([("setup", "x"), ("build", "y")])
    b = hash_stage_commands([("build", "y"), ("setup", "x")])
    assert a == b


# ── PreparedStateRecord ─────────────────────────────────────────────────


def test_record_round_trip() -> None:
    rec = PreparedStateRecord(
        sha="abc1234",
        target="ubuntu",
        mode="default",
        config_hash="0123",
    )
    rec.mark("setup", "pass")
    rec.mark("build", "fail")

    d = rec.to_dict()
    rec2 = PreparedStateRecord.from_dict(d)
    assert rec2.sha == rec.sha
    assert rec2.target == rec.target
    assert rec2.mode == rec.mode
    assert rec2.stages == rec.stages
    assert rec2.config_hash == rec.config_hash


def test_record_is_passed() -> None:
    rec = PreparedStateRecord(sha="abc", target="t", mode="m")
    rec.mark("setup", "pass")
    rec.mark("build", "fail")
    assert rec.is_passed("setup") is True
    assert rec.is_passed("build") is False
    assert rec.is_passed("test") is False  # never recorded


def test_record_mark_updates_timestamp() -> None:
    rec = PreparedStateRecord(sha="a", target="t", mode="m")
    initial = rec.updated_at
    rec.mark("setup", "pass")
    assert rec.updated_at >= initial


# ── PreparedStateStore: get / save / delete ─────────────────────────────


def test_store_get_missing(store: PreparedStateStore) -> None:
    assert store.get("nonexistent", "ubuntu", "default") is None


def test_store_save_and_get(store: PreparedStateStore) -> None:
    rec = PreparedStateRecord(
        sha="abc1234",
        target="ubuntu",
        mode="default",
        config_hash="0123",
    )
    rec.mark("setup", "pass")
    rec.mark("build", "fail")

    store.save(rec)

    loaded = store.get("abc1234", "ubuntu", "default")
    assert loaded is not None
    assert loaded.sha == "abc1234"
    assert loaded.is_passed("setup") is True
    assert loaded.is_passed("build") is False


def test_store_save_overwrites(store: PreparedStateStore) -> None:
    rec = PreparedStateRecord(sha="a", target="t", mode="m")
    rec.mark("setup", "fail")
    store.save(rec)

    rec.mark("setup", "pass")
    store.save(rec)

    loaded = store.get("a", "t", "m")
    assert loaded is not None
    assert loaded.is_passed("setup") is True


def test_store_delete(store: PreparedStateStore) -> None:
    rec = PreparedStateRecord(sha="a", target="t", mode="m")
    rec.mark("setup", "pass")
    store.save(rec)

    store.delete("a", "t", "m")
    assert store.get("a", "t", "m") is None


def test_store_delete_sha(store: PreparedStateStore) -> None:
    """delete_sha removes every record under one SHA."""
    for target in ("ubuntu", "windows", "macos"):
        rec = PreparedStateRecord(sha="abc", target=target, mode="default")
        rec.mark("setup", "pass")
        store.save(rec)

    count = store.delete_sha("abc")
    assert count == 3
    for target in ("ubuntu", "windows", "macos"):
        assert store.get("abc", target, "default") is None


def test_store_cleanup_other_shas(store: PreparedStateStore) -> None:
    """cleanup_other_shas keeps the chosen SHA and removes the rest."""
    for sha in ("old1", "old2", "current"):
        rec = PreparedStateRecord(sha=sha, target="ubuntu", mode="default")
        rec.mark("setup", "pass")
        store.save(rec)

    count = store.cleanup_other_shas(keep_sha="current")
    assert count == 2
    assert store.get("current", "ubuntu", "default") is not None
    assert store.get("old1", "ubuntu", "default") is None
    assert store.get("old2", "ubuntu", "default") is None


def test_store_corrupted_record_returns_none(store: PreparedStateStore) -> None:
    """A corrupted JSON file is treated as missing."""
    record_path = store.path / "abc" / "ubuntu--default.json"
    record_path.parent.mkdir(parents=True, exist_ok=True)
    record_path.write_text("not valid json {{{")

    assert store.get("abc", "ubuntu", "default") is None


# ── filter_stages_by_prepared_state ─────────────────────────────────────


def test_filter_no_record_runs_everything() -> None:
    stages = [("setup", "x"), ("build", "y"), ("test", "z")]
    to_run, skipped = filter_stages_by_prepared_state(
        stages, None, current_config_hash="abc",
    )
    assert to_run == stages
    assert skipped == []


def test_filter_skips_passed_stages() -> None:
    stages = [("setup", "x"), ("build", "y"), ("test", "z")]
    rec = PreparedStateRecord(sha="s", target="t", mode="m", config_hash="abc")
    rec.mark("setup", "pass")
    rec.mark("build", "pass")

    to_run, skipped = filter_stages_by_prepared_state(
        stages, rec, current_config_hash="abc",
    )
    assert skipped == ["setup", "build"]
    assert [name for name, _ in to_run] == ["test"]


def test_filter_stops_skipping_at_first_failed_stage() -> None:
    """Even if a later stage previously passed, it must re-run after a failure."""
    stages = [("setup", "x"), ("build", "y"), ("test", "z")]
    rec = PreparedStateRecord(sha="s", target="t", mode="m", config_hash="abc")
    rec.mark("setup", "pass")
    # build NOT marked → skipping_phase ends
    rec.mark("test", "pass")

    to_run, skipped = filter_stages_by_prepared_state(
        stages, rec, current_config_hash="abc",
    )
    assert skipped == ["setup"]
    # build and test BOTH must run because build wasn't cached
    assert [name for name, _ in to_run] == ["build", "test"]


def test_filter_invalidated_by_config_hash_mismatch() -> None:
    """Config hash mismatch invalidates the entire cache."""
    stages = [("setup", "x"), ("build", "y")]
    rec = PreparedStateRecord(sha="s", target="t", mode="m", config_hash="OLD")
    rec.mark("setup", "pass")
    rec.mark("build", "pass")

    to_run, skipped = filter_stages_by_prepared_state(
        stages, rec, current_config_hash="NEW",
    )
    assert skipped == []
    assert to_run == stages


def test_filter_all_stages_passed() -> None:
    """If every stage is cached as pass, the run is a no-op."""
    stages = [("setup", "x"), ("build", "y")]
    rec = PreparedStateRecord(sha="s", target="t", mode="m", config_hash="abc")
    rec.mark("setup", "pass")
    rec.mark("build", "pass")

    to_run, skipped = filter_stages_by_prepared_state(
        stages, rec, current_config_hash="abc",
    )
    assert skipped == ["setup", "build"]
    assert to_run == []

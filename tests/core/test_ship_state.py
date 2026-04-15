"""Tests for core.ship_state: durable in-flight ship state store."""

from __future__ import annotations

import json
import tempfile
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from shipyard.core.ship_state import (
    SCHEMA_VERSION,
    DispatchedRun,
    ShipState,
    ShipStateStore,
    compute_policy_signature,
)


@pytest.fixture
def store() -> ShipStateStore:
    with tempfile.TemporaryDirectory() as tmp:
        yield ShipStateStore(path=Path(tmp) / "ship")


def _make_state(pr: int = 224, sha: str = "abc1234") -> ShipState:
    return ShipState(
        pr=pr,
        repo="danielraffel/pulp",
        branch="feature/test",
        base_branch="main",
        head_sha=sha,
        policy_signature="policy0001",
    )


def _make_run(target: str = "cloud", run_id: str = "99999") -> DispatchedRun:
    now = datetime.now(timezone.utc)
    return DispatchedRun(
        target=target,
        provider="namespace",
        run_id=run_id,
        status="in_progress",
        started_at=now,
        updated_at=now,
    )


class TestDispatchedRun:
    def test_roundtrip(self) -> None:
        run = _make_run()
        assert DispatchedRun.from_dict(run.to_dict()) == run

    def test_run_id_coerced_to_string(self) -> None:
        # GitHub returns numeric IDs; upstream callers may pass ints.
        run = DispatchedRun.from_dict(
            {
                "target": "cloud",
                "provider": "namespace",
                "run_id": 24446948064,
                "status": "in_progress",
                "started_at": "2026-04-15T10:00:00+00:00",
                "updated_at": "2026-04-15T10:00:00+00:00",
            }
        )
        assert run.run_id == "24446948064"

    def test_attempt_defaults_to_one(self) -> None:
        d = _make_run().to_dict()
        d.pop("attempt", None)
        assert DispatchedRun.from_dict(d).attempt == 1


class TestShipState:
    def test_upsert_run_inserts_new(self) -> None:
        state = _make_state()
        state.upsert_run(_make_run(target="cloud"))
        state.upsert_run(_make_run(target="ubuntu"))
        assert len(state.dispatched_runs) == 2

    def test_upsert_run_replaces_same_target_and_id(self) -> None:
        state = _make_state()
        state.upsert_run(_make_run(run_id="111"))
        later = _make_run(run_id="111")
        later = DispatchedRun(
            target=later.target,
            provider=later.provider,
            run_id=later.run_id,
            status="completed",
            started_at=later.started_at,
            updated_at=later.updated_at,
        )
        state.upsert_run(later)
        assert len(state.dispatched_runs) == 1
        assert state.dispatched_runs[0].status == "completed"

    def test_upsert_run_different_id_keeps_both(self) -> None:
        # A rerun of the same target produces a new run_id — keep history.
        state = _make_state()
        state.upsert_run(_make_run(run_id="111"))
        state.upsert_run(_make_run(run_id="222"))
        assert len(state.dispatched_runs) == 2

    def test_get_run_returns_most_recent(self) -> None:
        state = _make_state()
        older = datetime.now(timezone.utc) - timedelta(minutes=10)
        newer = datetime.now(timezone.utc)
        state.dispatched_runs.append(
            DispatchedRun(
                target="cloud",
                provider="namespace",
                run_id="111",
                status="failed",
                started_at=older,
                updated_at=older,
            )
        )
        state.dispatched_runs.append(
            DispatchedRun(
                target="cloud",
                provider="namespace",
                run_id="222",
                status="in_progress",
                started_at=newer,
                updated_at=newer,
            )
        )
        assert state.get_run("cloud").run_id == "222"

    def test_get_run_returns_none_when_absent(self) -> None:
        assert _make_state().get_run("cloud") is None

    def test_update_evidence_sets_snapshot(self) -> None:
        state = _make_state()
        state.update_evidence("macos", "pass")
        assert state.evidence_snapshot == {"macos": "pass"}

    def test_is_sha_drift(self) -> None:
        state = _make_state(sha="abc")
        assert not state.is_sha_drift("abc")
        assert state.is_sha_drift("def")

    def test_touch_updates_timestamp(self) -> None:
        state = _make_state()
        original = state.updated_at
        # Sleep-free: directly rewind then touch.
        state.updated_at = original - timedelta(seconds=5)
        state.touch()
        assert state.updated_at > original - timedelta(seconds=5)

    def test_roundtrip_preserves_all_fields(self) -> None:
        state = _make_state()
        state.pr_url = "https://github.com/danielraffel/pulp/pull/224"
        state.pr_title = "Fix ARA controller"
        state.commit_subject = "ara: out-of-line destructor"
        state.upsert_run(_make_run())
        state.update_evidence("macos", "pass")
        restored = ShipState.from_dict(state.to_dict())
        assert restored.pr == state.pr
        assert restored.head_sha == state.head_sha
        assert restored.policy_signature == state.policy_signature
        assert restored.pr_url == state.pr_url
        assert restored.pr_title == state.pr_title
        assert restored.commit_subject == state.commit_subject
        assert len(restored.dispatched_runs) == 1
        assert restored.evidence_snapshot == {"macos": "pass"}
        assert restored.schema_version == SCHEMA_VERSION

    def test_legacy_files_without_pr_context_load(self) -> None:
        # A state file written by an earlier Shipyard version without
        # the human-context fields must still deserialize cleanly.
        data = _make_state().to_dict()
        for field_name in ("pr_url", "pr_title", "commit_subject"):
            data.pop(field_name, None)
        restored = ShipState.from_dict(data)
        assert restored.pr_url == ""
        assert restored.pr_title == ""
        assert restored.commit_subject == ""


class TestShipStateStore:
    def test_get_missing_returns_none(self, store: ShipStateStore) -> None:
        assert store.get(999) is None

    def test_save_then_get(self, store: ShipStateStore) -> None:
        state = _make_state()
        state.upsert_run(_make_run())
        store.save(state)
        restored = store.get(state.pr)
        assert restored is not None
        assert restored.pr == state.pr
        assert len(restored.dispatched_runs) == 1

    def test_save_preserves_updated_at(self, store: ShipStateStore) -> None:
        # save() writes what it is given; it is the caller's job to
        # update timestamps before save (which upsert_run/update_evidence
        # already do).
        state = _make_state()
        fixed = datetime(2026, 1, 1, tzinfo=timezone.utc)
        state.updated_at = fixed
        store.save(state)
        restored = store.get(state.pr)
        assert restored is not None and restored.updated_at == fixed

    def test_save_is_atomic_no_partial_file_on_corrupt_input(
        self, store: ShipStateStore
    ) -> None:
        # A corrupt existing file should not block a subsequent save,
        # and the read path should tolerate the intermediate state.
        store._state_path(224).write_text("{not valid json")
        assert store.get(224) is None
        store.save(_make_state(pr=224))
        assert store.get(224) is not None

    def test_corrupt_file_returns_none(self, store: ShipStateStore) -> None:
        store._state_path(101).write_text("{this is not json")
        assert store.get(101) is None

    def test_truncated_file_returns_none(self, store: ShipStateStore) -> None:
        store._state_path(102).write_text('{"pr": 102,')
        assert store.get(102) is None

    def test_missing_required_field_returns_none(
        self, store: ShipStateStore
    ) -> None:
        # A file missing a required key returns None rather than raising.
        store._state_path(103).write_text(json.dumps({"pr": 103}))
        assert store.get(103) is None

    def test_delete_removes_active(self, store: ShipStateStore) -> None:
        state = _make_state(pr=500)
        store.save(state)
        store.delete(500)
        assert store.get(500) is None

    def test_delete_missing_is_noop(self, store: ShipStateStore) -> None:
        # Should not raise.
        store.delete(999)

    def test_archive_moves_to_archive_dir(self, store: ShipStateStore) -> None:
        state = _make_state(pr=600)
        store.save(state)
        archived = store.archive(600)
        assert archived is not None
        assert archived.exists()
        assert archived.parent.name == "archive"
        assert store.get(600) is None

    def test_archive_returns_none_when_absent(self, store: ShipStateStore) -> None:
        assert store.archive(9999) is None

    def test_list_active_skips_archive_dir(self, store: ShipStateStore) -> None:
        store.save(_make_state(pr=1))
        store.save(_make_state(pr=2))
        store.archive(2)
        active = store.list_active()
        assert [s.pr for s in active] == [1]

    def test_list_active_skips_non_integer_names(
        self, store: ShipStateStore
    ) -> None:
        # A stray temp file should not crash list_active.
        (store.path / "notapr.json").write_text("{}")
        store.save(_make_state(pr=7))
        active = store.list_active()
        assert [s.pr for s in active] == [7]

    def test_list_archived(self, store: ShipStateStore) -> None:
        store.save(_make_state(pr=10))
        store.archive(10)
        assert len(store.list_archived()) == 1

    def test_list_active_ignores_corrupt_files(
        self, store: ShipStateStore
    ) -> None:
        store.save(_make_state(pr=20))
        store._state_path(21).write_text("{broken")
        active = store.list_active()
        assert [s.pr for s in active] == [20]

    def test_archive_and_replace_increments_attempt(
        self, store: ShipStateStore
    ) -> None:
        state = _make_state(pr=30)
        state.attempt = 1
        state.upsert_run(_make_run())
        state.update_evidence("macos", "pass")
        store.save(state)
        fresh = store.archive_and_replace(state)
        assert fresh.attempt == 2
        assert fresh.dispatched_runs == []
        assert fresh.evidence_snapshot == {}
        # The archive should have captured the prior state.
        assert len(store.list_archived()) == 1

    def test_second_instance_reads_same_data(
        self, store: ShipStateStore
    ) -> None:
        state = _make_state(pr=42)
        state.upsert_run(_make_run(run_id="999"))
        store.save(state)
        # Pretend the process restarted — open a new store at the same path.
        second = ShipStateStore(path=store.path)
        restored = second.get(42)
        assert restored is not None
        assert restored.dispatched_runs[0].run_id == "999"

    def test_no_temp_files_left_after_save(self, store: ShipStateStore) -> None:
        store.save(_make_state(pr=55))
        stray = list(store.path.glob(".*.tmp"))
        assert stray == []


class TestPrune:
    def test_prune_archived_older_than_cutoff(
        self, store: ShipStateStore
    ) -> None:
        store.save(_make_state(pr=1))
        archived = store.archive(1)
        assert archived is not None
        # Backdate the file mtime so it looks old.
        old = datetime.now(timezone.utc) - timedelta(days=60)
        os_stat_time = old.timestamp()
        import os

        os.utime(archived, (os_stat_time, os_stat_time))
        report = store.prune(archive_days=30)
        assert archived.name in report.deleted_archived
        assert not archived.exists()

    def test_prune_does_not_touch_recent_archive(
        self, store: ShipStateStore
    ) -> None:
        store.save(_make_state(pr=2))
        archived = store.archive(2)
        assert archived is not None
        report = store.prune(archive_days=30)
        assert report.total == 0
        assert archived.exists()

    def test_prune_active_requires_closed_pr_set(
        self, store: ShipStateStore
    ) -> None:
        state = _make_state(pr=3)
        state.updated_at = datetime.now(timezone.utc) - timedelta(days=90)
        store.save(state)
        # Without a closed_prs set, even very old active states survive —
        # they might still be in-flight.
        report = store.prune(active_days=14)
        assert state.pr not in report.deleted_active
        assert store.get(3) is not None

    def test_prune_active_deletes_only_closed_and_stale(
        self, store: ShipStateStore
    ) -> None:
        stale_closed = _make_state(pr=10)
        stale_closed.updated_at = datetime.now(timezone.utc) - timedelta(days=90)
        fresh_closed = _make_state(pr=11)
        stale_open = _make_state(pr=12)
        stale_open.updated_at = datetime.now(timezone.utc) - timedelta(days=90)
        for s in (stale_closed, fresh_closed, stale_open):
            store.save(s)
        report = store.prune(
            active_days=14, closed_prs={10, 11}
        )
        assert 10 in report.deleted_active  # stale + closed → gone
        assert 11 not in report.deleted_active  # fresh + closed → kept
        assert 12 not in report.deleted_active  # stale + open → kept
        assert store.get(10) is None
        assert store.get(11) is not None
        assert store.get(12) is not None

    def test_prune_report_total(self, store: ShipStateStore) -> None:
        report = store.prune()
        assert report.total == 0
        assert report.to_dict()["total"] == 0


class TestPolicySignature:
    def test_stable_across_input_order(self) -> None:
        a = compute_policy_signature(
            ["macos", "linux", "windows"], ["mac", "ubuntu"], "default"
        )
        b = compute_policy_signature(
            ["windows", "macos", "linux"], ["ubuntu", "mac"], "default"
        )
        assert a == b

    def test_changes_when_platform_added(self) -> None:
        a = compute_policy_signature(["macos"], ["mac"], "default")
        b = compute_policy_signature(["macos", "linux"], ["mac"], "default")
        assert a != b

    def test_changes_when_mode_changes(self) -> None:
        a = compute_policy_signature(["macos"], ["mac"], "default")
        b = compute_policy_signature(["macos"], ["mac"], "strict")
        assert a != b

    def test_signature_length(self) -> None:
        sig = compute_policy_signature(["macos"], ["mac"], "default")
        assert len(sig) == 16

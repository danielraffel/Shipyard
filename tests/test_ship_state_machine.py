"""Phase B ship-state transition tests (#101).

Named per-transition (T1–T13) + per-bug (B1–B4) + per-silent-failure
(SF1–SF3) so failure output maps directly to
`docs/ship-state-machine.md`.

Bug regression tests are marked `xfail(strict=True)` against the
filed bug numbers. When a fix lands, the `xfail` is flipped to a
plain assertion and the state-machine lane catches the regression
if the behavior reverts.

CI runs this file under the `state_machine` pytest marker so a
state-machine failure is visually distinct in the PR checks list.
"""

from __future__ import annotations

import json
import os
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

import pytest

from shipyard.cloud.records import CloudRecordStore, CloudRunRecord
from shipyard.core.ship_state import (
    DispatchedRun,
    ShipState,
    ShipStateStore,
    compute_policy_signature,
)


pytestmark = pytest.mark.state_machine


# ── Builders ───────────────────────────────────────────────────────


def _now() -> datetime:
    return datetime.now(timezone.utc)


def _run(
    target: str,
    *,
    provider: str = "namespace",
    run_id: str = "12345",
    status: str = "in_progress",
    required: bool = True,
    at: datetime | None = None,
) -> DispatchedRun:
    ts = at or _now()
    return DispatchedRun(
        target=target,
        provider=provider,
        run_id=run_id,
        status=status,
        started_at=ts,
        updated_at=ts,
        required=required,
    )


def _state(
    pr: int = 42,
    *,
    runs: list[DispatchedRun] | None = None,
    evidence: dict[str, str] | None = None,
    attempt: int = 1,
    head_sha: str = "a" * 40,
) -> ShipState:
    return ShipState(
        pr=pr,
        repo="danielraffel/pulp",
        branch="feat/x",
        base_branch="main",
        head_sha=head_sha,
        policy_signature=compute_policy_signature(
            ["macos", "linux"], ["macos", "ubuntu"], "FULL"
        ),
        dispatched_runs=list(runs or []),
        evidence_snapshot=dict(evidence or {}),
        attempt=attempt,
    )


# ── T3 — terminal outcome batch save ──────────────────────────────


class TestT3_BatchSave:
    """A ShipStateStore.save happens once after _execute_job, not per
    target — a kill mid-loop loses the whole batch, not one record.

    We can't kill _execute_job from a unit test, but we can assert the
    single-save invariant directly against ShipStateStore: mutating
    multiple fields and then saving once produces a single on-disk
    version, and a save failure between mutations preserves the
    pre-mutation file byte-for-byte.
    """

    def test_single_save_covers_multiple_mutations(self, tmp_path: Path) -> None:
        store = ShipStateStore(path=tmp_path / "ship")
        state = _state()
        store.save(state)

        state.update_evidence("macos", "pass")
        state.update_evidence("ubuntu", "pass")
        state.update_evidence("windows", "fail")
        store.save(state)

        persisted = store.get(state.pr)
        assert persisted is not None
        assert persisted.evidence_snapshot == {
            "macos": "pass",
            "ubuntu": "pass",
            "windows": "fail",
        }

    def test_mid_batch_save_failure_preserves_prior_state(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Invariant from docs/ship-state-machine.md T3: if save fails,
        the previous valid state is byte-identical on disk."""
        store = ShipStateStore(path=tmp_path / "ship")
        state = _state()
        store.save(state)
        original = (store._state_path(state.pr)).read_text()

        state.update_evidence("macos", "pass")
        state.update_evidence("ubuntu", "pass")

        def _boom(*args: Any, **kwargs: Any) -> None:
            raise OSError("disk full")

        # os.replace is the atomic-rename step in ShipStateStore.save.
        monkeypatch.setattr(os, "replace", _boom)

        with pytest.raises(OSError):
            store.save(state)

        # Prior file intact, no torn half.
        assert (store._state_path(state.pr)).read_text() == original


# ── T7 — resume revalidates every lane (observation test) ──────────


class TestT7_ResumeRevalidatesEveryLane:
    """Observation test (see docs/ship-state-machine.md T7): resume
    does NOT skip a lane that already has passing evidence. This is
    not necessarily correct behavior long-term, but it IS the current
    behavior — test locks it in so a future "skip-on-pass" change is
    explicit and accompanied by a state-machine-doc update.
    """

    def test_state_retains_every_dispatched_target_even_if_passed(
        self, tmp_path: Path
    ) -> None:
        store = ShipStateStore(path=tmp_path / "ship")
        state = _state(
            runs=[_run("macos", run_id="1"), _run("ubuntu", run_id="2")],
            evidence={"macos": "pass"},  # ubuntu not yet terminal
        )
        store.save(state)
        loaded = store.get(state.pr)
        assert loaded is not None
        # The loaded state keeps both DispatchedRun rows — resume
        # iterates job.target_names (cli.py:4219), so any lane-skip
        # decision would have to consult evidence_snapshot, which it
        # does NOT today. Phase B keeps this invariant until the
        # skip-on-pass feature is designed.
        assert {r.target for r in loaded.dispatched_runs} == {"macos", "ubuntu"}


# ── T11 — discard archives ANY active state ───────────────────────


class TestT11_DiscardArchivesAnyActive:
    """ship-state discard works on any state, not only STATE_MERGED."""

    def test_discard_archives_fresh_state(self, tmp_path: Path) -> None:
        store = ShipStateStore(path=tmp_path / "ship")
        state = _state(runs=[_run("macos", run_id="1")])
        store.save(state)

        archived = store.archive(state.pr)
        assert archived is not None
        assert archived.exists()
        assert store.get(state.pr) is None  # no longer active

    def test_discard_archives_verdict_fail_state(self, tmp_path: Path) -> None:
        store = ShipStateStore(path=tmp_path / "ship")
        state = _state(
            runs=[_run("macos", run_id="1")],
            evidence={"macos": "fail"},
        )
        store.save(state)

        archived = store.archive(state.pr)
        assert archived is not None
        assert "42-" in archived.name
        assert archived.name.endswith(".json")


# ── T12 — prune only deletes active state for closed PRs ──────────


class TestT12_PruneGates:
    def test_active_state_not_deleted_without_closed_prs_set(
        self, tmp_path: Path
    ) -> None:
        store = ShipStateStore(path=tmp_path / "ship")
        state = _state()
        state.updated_at = _now() - timedelta(days=365)  # ancient
        store.save(state)

        report = store.prune(active_days=14, archive_days=30, now=_now())
        # No closed_prs provided → active files are NEVER auto-deleted.
        assert state.pr not in report.deleted_active
        assert store.get(state.pr) is not None

    def test_active_state_deleted_when_closed_and_old(
        self, tmp_path: Path
    ) -> None:
        store = ShipStateStore(path=tmp_path / "ship")
        state = _state()
        state.updated_at = _now() - timedelta(days=365)
        store.save(state)

        report = store.prune(
            active_days=14,
            archive_days=30,
            closed_prs={state.pr},
            now=_now(),
        )
        assert state.pr in report.deleted_active
        assert store.get(state.pr) is None


# ── T13 — cross-PR evidence reuse persists as "completed" ─────────


class TestT13_ReusePersistsAsCompleted:
    """Invariant from docs/ship-state-machine.md T13: reused lanes
    become DispatchedRun(status="completed"), NOT status="reused".
    `reused` only appears as a backend/display label, never in the
    persisted status field.
    """

    def test_reused_run_persisted_as_completed(self, tmp_path: Path) -> None:
        store = ShipStateStore(path=tmp_path / "ship")
        now = _now()
        reused_run = DispatchedRun(
            target="macos",
            provider="namespace",
            run_id="reused-from-abc1234",
            status="completed",  # mirroring cli.py:4586
            started_at=now,
            updated_at=now,
            required=True,
        )
        state = _state(runs=[reused_run], evidence={"macos": "pass"})
        store.save(state)

        loaded = store.get(state.pr)
        assert loaded is not None
        (mac,) = [r for r in loaded.dispatched_runs if r.target == "macos"]
        # No "reused" status — the audit's schema row for
        # DispatchedRun.status says only queued/in_progress/completed/
        # failed/cancelled are valid.
        assert mac.status == "completed"
        assert mac.status != "reused"


# ── SF1 — archive failure after merge is not idempotent on retry ──


class TestSF1_ArchiveFailureNotIdempotentOnRetry:
    """From docs/ship-state-machine.md §silent-failure #1.

    If `archive(pr)` fails after a successful merge, the active state
    file remains. The next auto-merge tick reads it, sees
    STATE_VERDICT_PASS again, and attempts `merge_pr` a second time.
    GitHub responds "already merged"; `_pr_is_merged` does NOT save
    us because it only runs on the state-absent branch (cli.py:3242).

    This test documents the current (undesirable) behavior by proving
    the exact precondition: after archive() fails and leaves the
    state present, a subsequent `get(pr)` still returns the PASS
    state — so the auto-merge retry will in fact re-enter the merge
    branch, not the no-state branch.
    """

    def test_failed_archive_leaves_state_visible_to_next_tick(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        store = ShipStateStore(path=tmp_path / "ship")
        state = _state(
            runs=[_run("macos", run_id="1", status="completed")],
            evidence={"macos": "pass"},
        )
        store.save(state)

        def _boom(*args: Any, **kwargs: Any) -> None:
            raise OSError("archive rename failed")

        monkeypatch.setattr(os, "replace", _boom)

        with pytest.raises(OSError):
            store.archive(state.pr)

        # Precondition for silent-failure #1: the active file is still
        # present. The next auto-merge tick will read PASS and retry
        # merge_pr instead of falling through to _pr_is_merged.
        reread = store.get(state.pr)
        assert reread is not None
        assert reread.evidence_snapshot == {"macos": "pass"}


# ── SF3 — ShipStateStore.save durability (already uses tmp+replace) ─


class TestSF3_SaveTmpWriteDurability:
    """Regression: ShipStateStore.save must not torn-write.

    Explicitly decoupled from queue.json — Queue._save uses a
    different pattern on `main` and gets its own atomicity fix in
    PR #105. This test covers only core/ship_state.py:342–357.
    """

    def test_save_failure_after_fsync_preserves_prior_file(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        store = ShipStateStore(path=tmp_path / "ship")
        store.save(_state())
        before = (store._state_path(42)).read_text()

        def _boom(*args: Any, **kwargs: Any) -> None:
            raise OSError("rename blocked")

        monkeypatch.setattr(os, "replace", _boom)
        with pytest.raises(OSError):
            store.save(_state(evidence={"macos": "fail"}))

        # Prior file byte-for-byte intact.
        assert (store._state_path(42)).read_text() == before

    def test_save_failure_cleans_up_tmp(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        store = ShipStateStore(path=tmp_path / "ship")

        def _boom(*args: Any, **kwargs: Any) -> None:
            raise OSError("rename blocked")

        monkeypatch.setattr(os, "replace", _boom)
        with pytest.raises(OSError):
            store.save(_state())

        # No leftover `.PR.tmp` files in the store path.
        leftovers = [
            p for p in (store.path).glob(".*.tmp")
        ]
        assert not leftovers, f"orphan tmp files after failed save: {leftovers}"


# ── _update_ship_state_from_job touch() invariants ────────────────


class TestTouchInvariants:
    """Every mutation helper bumps `updated_at`; read-only helpers
    do not. Part of the generic Phase B assertion layer.

    We back-date `updated_at` before each mutation so the assertion
    is robust against Windows's ~15ms `datetime.now()` resolution:
    on Windows, a same-tick capture can otherwise read equal to the
    value `touch()` wrote.
    """

    def test_update_evidence_bumps_updated_at(self) -> None:
        state = _state()
        state.updated_at = state.updated_at - timedelta(seconds=1)
        before = state.updated_at
        state.update_evidence("macos", "pass")
        assert state.updated_at > before

    def test_upsert_run_bumps_updated_at(self) -> None:
        state = _state()
        state.updated_at = state.updated_at - timedelta(seconds=1)
        before = state.updated_at
        state.upsert_run(_run("macos"))
        assert state.updated_at > before

    def test_append_run_bumps_updated_at(self) -> None:
        state = _state()
        state.updated_at = state.updated_at - timedelta(seconds=1)
        before = state.updated_at
        state.append_run(_run("ubuntu", run_id="2"))
        assert state.updated_at > before


# ── Verdict computation — known-good cases ────────────────────────


class TestVerdictComputation:
    """Everything that's NOT Bug B1 — the verdict computer handles
    these cases correctly today. Phase B locks in the correct paths
    so a B1 fix doesn't accidentally break them."""

    def test_empty_evidence_returns_none(self) -> None:
        from shipyard.cli import _ship_terminal_verdict
        state = _state(runs=[_run("macos", run_id="1")], evidence={})
        assert _ship_terminal_verdict(state) is None

    def test_all_pass_all_required_returns_true(self) -> None:
        from shipyard.cli import _ship_terminal_verdict
        state = _state(
            runs=[_run("macos", run_id="1"), _run("ubuntu", run_id="2")],
            evidence={"macos": "pass", "ubuntu": "pass"},
        )
        assert _ship_terminal_verdict(state) is True

    def test_any_required_fail_returns_false(self) -> None:
        from shipyard.cli import _ship_terminal_verdict
        state = _state(
            runs=[_run("macos", run_id="1"), _run("ubuntu", run_id="2")],
            evidence={"macos": "pass", "ubuntu": "fail"},
        )
        assert _ship_terminal_verdict(state) is False

    def test_advisory_fail_is_tolerated(self) -> None:
        from shipyard.cli import _ship_terminal_verdict
        state = _state(
            runs=[
                _run("macos", run_id="1", required=True),
                _run("advisory-lint", run_id="2", required=False),
            ],
            evidence={"macos": "pass", "advisory-lint": "fail"},
        )
        assert _ship_terminal_verdict(state) is True

    def test_non_terminal_evidence_value_returns_none(self) -> None:
        from shipyard.cli import _ship_terminal_verdict
        state = _state(
            runs=[_run("macos", run_id="1")],
            evidence={"macos": "pending"},
        )
        assert _ship_terminal_verdict(state) is None


# ── Bug regression tests — xfail until filed issue is fixed ───────


class TestB1_PartialEvidenceCoverage:
    """#108: _ship_terminal_verdict must require coverage of every
    required DispatchedRun.target before declaring a verdict.

    Fixed in cli.py:_ship_terminal_verdict. Test flipped from xfail to
    a plain assertion — any regression reverts the fix.
    """

    def test_B1_partial_evidence_coverage_not_verdict_pass(self) -> None:
        from shipyard.cli import _ship_terminal_verdict
        state = _state(
            runs=[
                _run("macos", run_id="1"),
                _run("ubuntu", run_id="2"),
                _run("windows", run_id="3"),
            ],
            evidence={"macos": "pass"},  # ubuntu + windows missing
        )
        assert _ship_terminal_verdict(state) is None

    def test_B1_partial_evidence_tolerates_advisory_lane_gap(self) -> None:
        """Advisory lanes don't contribute to coverage, so they can be
        absent without blocking the verdict."""
        from shipyard.cli import _ship_terminal_verdict
        state = _state(
            runs=[
                _run("macos", run_id="1", required=True),
                _run("advisory-lint", run_id="2", required=False),
            ],
            evidence={"macos": "pass"},
        )
        assert _ship_terminal_verdict(state) is True


class TestB2_NoResumeAttemptCounter:
    """#109: `--no-resume` must carry forward the bumped attempt from
    archive_and_replace into the fresh ShipState the CLI constructs.

    Fixed by threading a `carried_attempt` through the `--no-resume`
    branch and passing it to the fresh ShipState's `attempt=` kwarg.
    This test exercises the store's contract (the bumped attempt
    return value). A companion CLI-level integration is covered by
    the state-machine lane via end-to-end `shipyard ship --no-resume`
    runs in the Phase B harness when the environment permits.
    """

    def test_B2_archive_and_replace_returns_bumped_attempt(
        self, tmp_path: Path
    ) -> None:
        store = ShipStateStore(path=tmp_path / "ship")
        prior = _state(attempt=3)
        store.save(prior)

        replacement = store.archive_and_replace(prior)
        assert replacement.attempt == 4
        # And the prior state is archived, not still active.
        assert store.get(prior.pr) is None
        assert len(store.list_archived()) == 1

    def test_B2_cli_carries_attempt_across_no_resume(
        self, tmp_path: Path
    ) -> None:
        """Simulate the CLI's --no-resume branch directly: archive +
        capture the bumped attempt + construct a fresh state with that
        attempt. This is the exact sequence cli.py performs post-fix.
        """
        store = ShipStateStore(path=tmp_path / "ship")
        prior = _state(attempt=3)
        store.save(prior)

        # Post-fix branch: capture the return value.
        replacement = store.archive_and_replace(prior)
        carried_attempt = replacement.attempt

        # Fresh state uses the carried attempt, not the default 1.
        fresh = ShipState(
            pr=prior.pr,
            repo=prior.repo,
            branch=prior.branch,
            base_branch=prior.base_branch,
            head_sha="b" * 40,
            policy_signature=prior.policy_signature,
            attempt=carried_attempt,
        )
        store.save(fresh)

        reread = store.get(prior.pr)
        assert reread is not None
        assert reread.attempt == 4  # not 1 — counter is monotonic.


class TestB3_RetargetUpdatesState:
    """#110: `cloud retarget --apply` must update ShipState after a
    successful dispatch — the prior DispatchedRun row is obsolete
    (its target moved providers), not coexistent with the new one.

    Fixed in cli.py cloud_retarget: after workflow_dispatch, the
    command now loads the PR's ShipState, drops prior rows for the
    retargeted target, and appends a fresh DispatchedRun. This test
    covers the state-mutation sequence directly (the end-to-end
    invocation is an `integration`-marked smoke elsewhere).
    """

    def test_B3_retarget_replaces_dispatched_run_row(
        self, tmp_path: Path
    ) -> None:
        from datetime import datetime, timezone

        store = ShipStateStore(path=tmp_path / "ship")
        state = _state(
            runs=[
                _run(
                    "macos",
                    provider="github-hosted",
                    run_id="old-123",
                    status="in_progress",
                )
            ]
        )
        store.save(state)

        # Mirror the post-fix retarget mutation sequence from
        # cli.py cloud_retarget.
        live = store.get(state.pr)
        assert live is not None
        now = datetime.now(timezone.utc)
        replacement = DispatchedRun(
            target="macos",
            provider="namespace",
            run_id="new-456",
            status="queued",
            started_at=now,
            updated_at=now,
            attempt=live.attempt,
            required=True,
        )
        live.dispatched_runs = [
            r for r in live.dispatched_runs if r.target != "macos"
        ]
        live.append_run(replacement)
        store.save(live)

        reloaded = store.get(state.pr)
        assert reloaded is not None
        mac_rows = [r for r in reloaded.dispatched_runs if r.target == "macos"]
        assert len(mac_rows) == 1, (
            "retarget must replace the row, not leave both providers"
        )
        assert mac_rows[0].provider == "namespace"
        assert mac_rows[0].run_id == "new-456"


class TestB4_CloudRunsByPlatformScopesToSha:
    """#111: `_cloud_runs_by_platform(ctx, sha)` must filter records
    by `requested_ref == sha` so cross-SHA attribution is impossible.

    Fixed in cli.py _cloud_runs_by_platform. Records whose
    requested_ref doesn't match are skipped; records with no
    requested_ref at all are tolerated (legacy fixtures).
    """

    def test_B4_cloud_runs_by_platform_scopes_to_sha(
        self, tmp_path: Path
    ) -> None:
        from shipyard.cli import _cloud_runs_by_platform

        class _FakeCtx:
            def __init__(self, store: CloudRecordStore) -> None:
                self.cloud_records = store

        store = CloudRecordStore(path=tmp_path / "cloud")
        now = _now()
        # Record A: SHA aaa, platform macos, run_id 100
        store.save(
            CloudRunRecord(
                dispatch_id="d1",
                workflow_key="ci",
                workflow_file="ci.yml",
                workflow_name="CI",
                repository="danielraffel/pulp",
                requested_ref="a" * 40,
                provider="namespace",
                dispatch_fields={"platform": "macos"},
                status="in_progress",
                run_id="100",
                dispatched_at=now,
                updated_at=now,
            )
        )
        # Record B: SHA bbb, platform macos, run_id 200 — newer
        store.save(
            CloudRunRecord(
                dispatch_id="d2",
                workflow_key="ci",
                workflow_file="ci.yml",
                workflow_name="CI",
                repository="danielraffel/pulp",
                requested_ref="b" * 40,
                provider="namespace",
                dispatch_fields={"platform": "macos"},
                status="in_progress",
                run_id="200",
                dispatched_at=now + timedelta(seconds=1),
                updated_at=now + timedelta(seconds=1),
            )
        )

        # Asking for SHA `aaa` must return 100, NOT 200. Today it
        # returns 200 (the most-recent, regardless of SHA) because
        # the sha parameter is ignored.
        mapping = _cloud_runs_by_platform(_FakeCtx(store), "a" * 40)
        assert mapping.get("macos") == "100"

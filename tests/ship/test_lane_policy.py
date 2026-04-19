"""Tests for lane degrade-mode — advisory vs required lanes.

These cover:
  * Parsing ``Lane-Policy:`` trailers out of commit messages.
  * Composing config-level advisory flags with a trailer overlay.
  * Merge-gate behaviour: an advisory red lane merges; an escalated
    advisory lane with the trailer blocks the merge.
  * ``DispatchedRun.required`` round-trips through to/from_dict.
"""

from __future__ import annotations

from datetime import datetime, timezone
from typing import TYPE_CHECKING

from shipyard.core.config import Config
from shipyard.core.evidence import EvidenceRecord
from shipyard.core.ship_state import DispatchedRun
from shipyard.ship.lane_policy import (
    LanePolicy,
    advisory_platforms_for_config,
    advisory_targets_from_config,
    parse_trailer,
    resolve_lane_policy,
)
from shipyard.ship.merge import can_merge
from shipyard.targets import is_advisory, parse_target

if TYPE_CHECKING:
    from shipyard.core.evidence import EvidenceStore


# ── parse_target / is_advisory ─────────────────────────────────────


class TestTargetAdvisoryField:
    def test_default_false(self) -> None:
        t = parse_target("mac", {"platform": "macos-arm64"})
        assert t.advisory is False

    def test_explicit_true(self) -> None:
        t = parse_target(
            "windows", {"platform": "windows-arm64", "advisory": True}
        )
        assert t.advisory is True

    def test_string_true_coerces(self) -> None:
        t = parse_target("windows", {"advisory": "true"})
        assert t.advisory is True

    def test_garbage_coerces_to_false(self) -> None:
        t = parse_target("windows", {"advisory": "maybe"})
        assert t.advisory is False

    def test_is_advisory_helper(self) -> None:
        assert is_advisory({"advisory": True}) is True
        assert is_advisory({"advisory": False}) is False
        assert is_advisory({}) is False


# ── parse_trailer ──────────────────────────────────────────────────


class TestParseTrailer:
    def test_no_trailer(self) -> None:
        assert parse_trailer("feat: something\n\nbody") == {}

    def test_single_advisory(self) -> None:
        msg = "feat: thing\n\nLane-Policy: windows=advisory\n"
        assert parse_trailer(msg) == {"windows": "advisory"}

    def test_single_required(self) -> None:
        msg = "feat: thing\n\nLane-Policy: windows=required\n"
        assert parse_trailer(msg) == {"windows": "required"}

    def test_multiple_pairs_on_one_line(self) -> None:
        msg = (
            "feat: thing\n\n"
            "Lane-Policy: windows=advisory macos=required\n"
        )
        assert parse_trailer(msg) == {
            "windows": "advisory",
            "macos": "required",
        }

    def test_comma_separated(self) -> None:
        msg = (
            "feat: thing\n\n"
            "Lane-Policy: windows=advisory, linux=required\n"
        )
        assert parse_trailer(msg) == {
            "windows": "advisory",
            "linux": "required",
        }

    def test_later_line_wins(self) -> None:
        msg = (
            "feat: thing\n\n"
            "Lane-Policy: windows=advisory\n"
            "Lane-Policy: windows=required\n"
        )
        assert parse_trailer(msg) == {"windows": "required"}

    def test_case_insensitive_key(self) -> None:
        msg = "feat: thing\n\nlane-policy: windows=advisory\n"
        assert parse_trailer(msg) == {"windows": "advisory"}

    def test_ignores_unknown_value(self) -> None:
        msg = "feat: thing\n\nLane-Policy: windows=maybe\n"
        assert parse_trailer(msg) == {}


# ── resolve_lane_policy ────────────────────────────────────────────


def _config_with_targets(targets: dict[str, dict]) -> Config:
    return Config(data={"targets": targets})


class TestResolveLanePolicy:
    def test_no_advisory_at_all(self) -> None:
        cfg = _config_with_targets(
            {"mac": {"platform": "macos-arm64"}}
        )
        policy = resolve_lane_policy(cfg, commit_message="feat: x")
        assert policy.advisory_targets == frozenset()
        assert policy.overrides_from_trailer == frozenset()

    def test_config_advisory_only(self) -> None:
        cfg = _config_with_targets(
            {
                "mac": {"platform": "macos-arm64"},
                "windows": {"platform": "windows-arm64", "advisory": True},
            }
        )
        policy = resolve_lane_policy(cfg, commit_message="")
        assert policy.advisory_targets == frozenset({"windows"})
        assert policy.overrides_from_trailer == frozenset()

    def test_trailer_escalates_to_required(self) -> None:
        cfg = _config_with_targets(
            {
                "windows": {"platform": "windows-arm64", "advisory": True},
            }
        )
        policy = resolve_lane_policy(
            cfg,
            commit_message="feat\n\nLane-Policy: windows=required\n",
            known_targets=["windows"],
        )
        assert policy.advisory_targets == frozenset()
        assert policy.overrides_from_trailer == frozenset({"windows"})

    def test_trailer_demotes_to_advisory(self) -> None:
        cfg = _config_with_targets(
            {"mac": {"platform": "macos-arm64"}}
        )
        policy = resolve_lane_policy(
            cfg,
            commit_message="feat\n\nLane-Policy: mac=advisory\n",
            known_targets=["mac"],
        )
        assert policy.advisory_targets == frozenset({"mac"})
        assert policy.overrides_from_trailer == frozenset({"mac"})

    def test_trailer_for_unknown_target_is_ignored(self) -> None:
        cfg = _config_with_targets(
            {"mac": {"platform": "macos-arm64"}}
        )
        policy = resolve_lane_policy(
            cfg,
            commit_message="Lane-Policy: ghost=advisory\n",
            known_targets=["mac"],
        )
        assert policy.advisory_targets == frozenset()
        assert policy.overrides_from_trailer == frozenset()

    def test_trailer_policy_matching_config_is_noop(self) -> None:
        cfg = _config_with_targets(
            {"windows": {"platform": "windows-arm64", "advisory": True}}
        )
        # Trailer says "advisory" but config already says advisory →
        # no override.
        policy = resolve_lane_policy(
            cfg,
            commit_message="Lane-Policy: windows=advisory\n",
            known_targets=["windows"],
        )
        assert policy.advisory_targets == frozenset({"windows"})
        assert policy.overrides_from_trailer == frozenset()


class TestAdvisoryTargetsFromConfig:
    def test_extract(self) -> None:
        cfg = _config_with_targets(
            {
                "mac": {"platform": "macos-arm64"},
                "windows": {"platform": "windows-arm64", "advisory": True},
                "linux": {"platform": "linux-x64", "advisory": False},
            }
        )
        assert advisory_targets_from_config(cfg) == {"windows"}


class TestAdvisoryPlatformsForConfig:
    def test_maps_targets_to_platforms(self) -> None:
        cfg = _config_with_targets(
            {
                "mac": {"platform": "macos-arm64"},
                "windows": {"platform": "windows-arm64", "advisory": True},
            }
        )
        assert advisory_platforms_for_config(
            cfg, commit_message=""
        ) == {"windows-arm64"}

    def test_trailer_overlay_flows_through(self) -> None:
        cfg = _config_with_targets(
            {
                "mac": {"platform": "macos-arm64"},
                "windows": {"platform": "windows-arm64", "advisory": True},
            }
        )
        # Escalate windows → required, so it shouldn't be in the
        # advisory set anymore.
        assert advisory_platforms_for_config(
            cfg,
            commit_message="Lane-Policy: windows=required\n",
        ) == set()


# ── merge gate ─────────────────────────────────────────────────────


def _record(
    store: EvidenceStore,
    *,
    target: str,
    platform: str,
    status: str,
    failure_class: str | None = None,
    sha: str = "abc123",
    branch: str = "feature/x",
) -> None:
    store.record(
        EvidenceRecord(
            sha=sha,
            branch=branch,
            target_name=target,
            platform=platform,
            status=status,
            backend="local",
            completed_at=datetime.now(timezone.utc),
            failure_class=failure_class,
        )
    )


class TestMergeGateWithAdvisoryLane:
    """The four-quadrant table:

    ┌─────────────┬─────────────┬──────────────┐
    │             │ Required    │ Advisory     │
    ├─────────────┼─────────────┼──────────────┤
    │ Passing     │ merge       │ merge        │
    │ Failing     │ BLOCK       │ merge (info) │
    │ Missing     │ BLOCK       │ merge (info) │
    └─────────────┴─────────────┴──────────────┘
    """

    def test_advisory_fail_allows_merge(
        self, evidence_store: EvidenceStore
    ) -> None:
        _record(
            evidence_store,
            target="mac", platform="macos-arm64", status="pass",
        )
        _record(
            evidence_store,
            target="windows", platform="windows-arm64",
            status="fail", failure_class="TEST",
        )
        check = can_merge(
            evidence_store,
            "feature/x", "abc123",
            ["macos-arm64", "windows-arm64"],
            advisory_platforms={"windows-arm64"},
        )
        assert check.ready is True
        assert check.passing == ["macos-arm64"]
        assert check.advisory == ["windows-arm64"]
        assert check.failing == []

    def test_advisory_infra_fail_also_allows_merge(
        self, evidence_store: EvidenceStore
    ) -> None:
        # Unlike quarantine (which never suppresses INFRA), advisory
        # mode is a strict policy knob: everything non-blocking.
        _record(
            evidence_store,
            target="mac", platform="macos-arm64", status="pass",
        )
        _record(
            evidence_store,
            target="windows", platform="windows-arm64",
            status="fail", failure_class="INFRA",
        )
        check = can_merge(
            evidence_store,
            "feature/x", "abc123",
            ["macos-arm64", "windows-arm64"],
            advisory_platforms={"windows-arm64"},
        )
        assert check.ready is True
        assert check.advisory == ["windows-arm64"]

    def test_required_fail_still_blocks(
        self, evidence_store: EvidenceStore
    ) -> None:
        # Baseline — with NO advisory, a TEST failure blocks.
        _record(
            evidence_store,
            target="mac", platform="macos-arm64",
            status="fail", failure_class="TEST",
        )
        check = can_merge(
            evidence_store,
            "feature/x", "abc123",
            ["macos-arm64"],
        )
        assert check.ready is False
        assert check.failing == ["macos-arm64"]
        assert check.advisory == []

    def test_missing_advisory_evidence_does_not_block(
        self, evidence_store: EvidenceStore
    ) -> None:
        _record(
            evidence_store,
            target="mac", platform="macos-arm64", status="pass",
        )
        # No record for windows-arm64.
        check = can_merge(
            evidence_store,
            "feature/x", "abc123",
            ["macos-arm64", "windows-arm64"],
            advisory_platforms={"windows-arm64"},
        )
        assert check.ready is True
        # Missing advisory evidence is surfaced as advisory, not missing.
        assert "windows-arm64" in check.advisory
        assert check.missing == []

    def test_missing_required_evidence_still_blocks(
        self, evidence_store: EvidenceStore
    ) -> None:
        _record(
            evidence_store,
            target="mac", platform="macos-arm64", status="pass",
        )
        check = can_merge(
            evidence_store,
            "feature/x", "abc123",
            ["macos-arm64", "windows-arm64"],
        )
        assert check.ready is False
        assert check.missing == ["windows-arm64"]


# ── DispatchedRun.required round-trip ──────────────────────────────


class TestDispatchedRunRequiredField:
    def test_default_true(self) -> None:
        now = datetime.now(timezone.utc)
        run = DispatchedRun(
            target="mac", provider="local", run_id="1",
            status="queued", started_at=now, updated_at=now,
        )
        assert run.required is True

    def test_advisory_lane_round_trip(self) -> None:
        now = datetime.now(timezone.utc)
        run = DispatchedRun(
            target="windows", provider="namespace", run_id="42",
            status="completed", started_at=now, updated_at=now,
            required=False,
        )
        d = run.to_dict()
        assert d["required"] is False
        restored = DispatchedRun.from_dict(d)
        assert restored.required is False

    def test_from_dict_legacy_state_is_required(self) -> None:
        # State files written before lane policy existed don't have
        # the `required` key. They must load as required=True so
        # existing ships don't silently degrade.
        now_iso = datetime.now(timezone.utc).isoformat()
        legacy = {
            "target": "mac",
            "provider": "local",
            "run_id": "1",
            "status": "completed",
            "started_at": now_iso,
            "updated_at": now_iso,
            "attempt": 1,
        }
        run = DispatchedRun.from_dict(legacy)
        assert run.required is True


# ── LanePolicy shape ───────────────────────────────────────────────


class TestLanePolicyShape:
    def test_is_advisory_is_required_inverse(self) -> None:
        p = LanePolicy(
            advisory_targets=frozenset({"windows"}),
            overrides_from_trailer=frozenset(),
        )
        assert p.is_advisory("windows") is True
        assert p.is_required("windows") is False
        assert p.is_advisory("mac") is False
        assert p.is_required("mac") is True

"""Truth-condition evaluators for `shipyard wait`.

Pure unit tests — no `gh`, no daemon, no network. Fixture dicts
stand in for what `wait_transport.fetch_*_snapshot` would return.
"""

from __future__ import annotations

import pytest

from shipyard.ship import wait

# ---------------------------------------------------------------------------
# release


def test_release_missing_snapshot_is_not_matched() -> None:
    result = wait.evaluate_release(None, manifest=None)
    assert result.matched is False
    assert result.observed == {"exists": False}


def test_release_draft_never_matches() -> None:
    result = wait.evaluate_release(
        {
            "tag_name": "v1",
            "draft": True,
            "assets": [{"name": "a", "state": "uploaded", "size": 1}],
        },
        manifest=["a"],
    )
    assert result.matched is False
    assert result.observed["draft"] is True


def test_release_with_manifest_matches_only_when_all_uploaded() -> None:
    snapshot = {
        "tag_name": "v1",
        "draft": False,
        "assets": [
            {"name": "linux", "state": "uploaded", "size": 10},
            {"name": "darwin", "state": "starter", "size": 0},
        ],
    }
    pending = wait.evaluate_release(snapshot, manifest=["linux", "darwin"])
    assert pending.matched is False
    assert pending.observed["not_uploaded"] == ["darwin"]

    completed_snapshot = dict(snapshot)
    completed_snapshot["assets"] = [
        {"name": "linux", "state": "uploaded", "size": 10},
        {"name": "darwin", "state": "uploaded", "size": 20},
    ]
    completed = wait.evaluate_release(
        completed_snapshot, manifest=["linux", "darwin"]
    )
    assert completed.matched is True
    assert completed.observed["not_uploaded"] == []
    assert completed.observed["missing"] == []


def test_release_with_missing_manifest_asset_not_matched() -> None:
    snapshot = {
        "tag_name": "v1",
        "draft": False,
        "assets": [{"name": "linux", "state": "uploaded", "size": 1}],
    }
    result = wait.evaluate_release(snapshot, manifest=["linux", "windows"])
    assert result.matched is False
    assert result.observed["missing"] == ["windows"]


def test_release_without_manifest_matches_any_uploaded_asset() -> None:
    snapshot = {
        "tag_name": "v1",
        "draft": False,
        "assets": [{"name": "a", "state": "uploaded", "size": 1}],
    }
    assert wait.evaluate_release(snapshot, manifest=None).matched is True

    empty = {
        "tag_name": "v1",
        "draft": False,
        "assets": [],
    }
    assert wait.evaluate_release(empty, manifest=None).matched is False


# ---------------------------------------------------------------------------
# pr --state green


def _rollup_entry(
    name: str,
    *,
    conclusion: str = "SUCCESS",
    state: str = "COMPLETED",
    required: bool = True,
) -> dict:
    return {
        "name": name,
        "conclusion": conclusion,
        "state": state,
        "isRequired": required,
    }


def test_pr_green_matches_when_all_required_pass() -> None:
    snapshot = {
        "number": 151,
        "headRefOid": "abc123",
        "state": "OPEN",
        "mergeable": "MERGEABLE",
        "mergeStateStatus": "CLEAN",
        "statusCheckRollup": [
            _rollup_entry("Linux"),
            _rollup_entry("Windows"),
            _rollup_entry("macOS"),
        ],
    }
    result = wait.evaluate_pr_green(snapshot)
    assert result.matched is True
    assert result.observed["head_sha"] == "abc123"
    assert len(result.observed["checks"]) == 3


def test_pr_green_does_not_match_when_still_waiting() -> None:
    snapshot = {
        "number": 151,
        "headRefOid": "abc123",
        "state": "OPEN",
        "mergeable": "UNKNOWN",
        "mergeStateStatus": "BLOCKED",
        "statusCheckRollup": [
            _rollup_entry("Linux"),
            _rollup_entry("Windows", conclusion="", state="IN_PROGRESS"),
        ],
    }
    result = wait.evaluate_pr_green(snapshot)
    assert result.matched is False


def test_pr_green_classifies_neutral_skipped_as_passing() -> None:
    snapshot = {
        "number": 1,
        "headRefOid": "x",
        "mergeable": "MERGEABLE",
        "mergeStateStatus": "CLEAN",
        "statusCheckRollup": [
            _rollup_entry("a", conclusion="NEUTRAL"),
            _rollup_entry("b", conclusion="SKIPPED"),
        ],
    }
    assert wait.evaluate_pr_green(snapshot).matched is True


def test_pr_green_with_failing_required_check_does_not_match() -> None:
    snapshot = {
        "number": 1,
        "headRefOid": "x",
        "mergeable": "MERGEABLE",
        "mergeStateStatus": "BLOCKED",
        "statusCheckRollup": [
            _rollup_entry("a", conclusion="FAILURE"),
        ],
    }
    assert wait.evaluate_pr_green(snapshot).matched is False


def test_pr_green_advisory_checks_ignored() -> None:
    snapshot = {
        "number": 1,
        "headRefOid": "x",
        "mergeable": "MERGEABLE",
        "mergeStateStatus": "CLEAN",
        "statusCheckRollup": [
            _rollup_entry("Required", conclusion="SUCCESS", required=True),
            _rollup_entry("Coverage", conclusion="FAILURE", required=False),
        ],
    }
    result = wait.evaluate_pr_green(snapshot)
    assert result.matched is True
    assert len(result.observed["checks"]) == 1
    assert len(result.observed["advisory"]) == 1


def test_pr_green_rulesets_detected_raises_unsupported_scope() -> None:
    snapshot = {
        "number": 1,
        "headRefOid": "x",
        "mergeable": "BLOCKED",
        "mergeStateStatus": "BLOCKED_BY_RULESET",
        "statusCheckRollup": [],
    }
    with pytest.raises(wait.UnsupportedScopeError):
        wait.evaluate_pr_green(snapshot)


def test_pr_green_merge_queue_raises_unsupported_scope() -> None:
    snapshot = {
        "number": 1,
        "headRefOid": "x",
        "mergeable": "BLOCKED",
        "mergeStateStatus": "MERGE_QUEUED",
        "statusCheckRollup": [],
    }
    with pytest.raises(wait.UnsupportedScopeError):
        wait.evaluate_pr_green(snapshot)


def test_pr_green_invalid_input_raises_for_missing_snapshot() -> None:
    with pytest.raises(wait.InvalidInputError):
        wait.evaluate_pr_green(None)


# ---------------------------------------------------------------------------
# pr --state merged | closed


def test_pr_state_merged_only_true_when_merged_flag_set() -> None:
    open_snapshot = {"number": 1, "state": "OPEN", "merged": False}
    assert wait.evaluate_pr_state(open_snapshot, target_state="merged").matched is False
    merged_snapshot = {"number": 1, "state": "CLOSED", "merged": True}
    assert wait.evaluate_pr_state(merged_snapshot, target_state="merged").matched is True


def test_pr_state_closed_matches_both_merged_and_unmerged_close() -> None:
    assert wait.evaluate_pr_state(
        {"number": 1, "state": "CLOSED", "merged": False},
        target_state="closed",
    ).matched is True
    assert wait.evaluate_pr_state(
        {"number": 1, "state": "MERGED", "merged": True},
        target_state="closed",
    ).matched is True
    assert wait.evaluate_pr_state(
        {"number": 1, "state": "OPEN", "merged": False},
        target_state="closed",
    ).matched is False


# ---------------------------------------------------------------------------
# run


def test_run_not_terminal_not_matched() -> None:
    r = wait.evaluate_run(
        {"databaseId": 1, "status": "in_progress", "conclusion": None},
        require_success=True,
    )
    assert r.matched is False


def test_run_terminal_without_success_flag_matches() -> None:
    r = wait.evaluate_run(
        {"databaseId": 1, "status": "completed", "conclusion": "failure"},
        require_success=False,
    )
    assert r.matched is True


def test_run_success_matches_when_conclusion_success() -> None:
    r = wait.evaluate_run(
        {"databaseId": 1, "status": "completed", "conclusion": "success"},
        require_success=True,
    )
    assert r.matched is True


def test_run_success_fails_fast_on_terminal_wrong_conclusion() -> None:
    with pytest.raises(wait.RunFailedFastError) as exc:
        wait.evaluate_run(
            {"databaseId": 1, "status": "completed", "conclusion": "failure"},
            require_success=True,
        )
    assert exc.value.observed["conclusion"] == "failure"


def test_run_missing_snapshot_raises_invalid_input() -> None:
    with pytest.raises(wait.InvalidInputError):
        wait.evaluate_run(None, require_success=False)

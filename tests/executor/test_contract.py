"""Tests for the validation contract evaluator."""

from __future__ import annotations

from shipyard.executor.contract import (
    evaluate_contract,
    required_markers,
)

# ── required_markers helper ─────────────────────────────────────────────


def test_required_markers_none() -> None:
    assert required_markers(None) == ()


def test_required_markers_empty() -> None:
    assert required_markers({}) == ()
    assert required_markers({"markers": []}) == ()


def test_required_markers_returns_tuple() -> None:
    cfg = {"markers": ["__PULP_VALIDATION__:smoke", "__PULP_VALIDATION__:full"]}
    assert required_markers(cfg) == (
        "__PULP_VALIDATION__:smoke",
        "__PULP_VALIDATION__:full",
    )


# ── No contract → no-op evaluation ──────────────────────────────────────


def test_evaluate_contract_no_config() -> None:
    result = evaluate_contract(None, ("anything",))
    assert result.violated is False
    assert result.enforce is False
    assert result.message is None
    assert result.missing == ()
    assert result.should_force_fail is False


def test_evaluate_contract_empty_config() -> None:
    result = evaluate_contract({}, ("anything",))
    assert result.violated is False
    assert result.message is None


def test_evaluate_contract_no_markers_declared() -> None:
    result = evaluate_contract({"enforce": True, "markers": []}, ("anything",))
    assert result.violated is False


# ── require_at_least_one mode (default) ─────────────────────────────────


def test_at_least_one_satisfied() -> None:
    cfg = {"markers": ["__PULP_VALIDATION__:smoke", "__PULP_VALIDATION__:full"]}
    result = evaluate_contract(cfg, ("__PULP_VALIDATION__:smoke",))
    assert result.violated is False
    assert result.missing == ()
    assert result.message is None
    assert result.should_force_fail is False


def test_at_least_one_violated() -> None:
    cfg = {"markers": ["__PULP_VALIDATION__:smoke", "__PULP_VALIDATION__:full"]}
    result = evaluate_contract(cfg, ())
    assert result.violated is True
    assert result.message is not None
    assert "at least one" in result.message
    assert "__PULP_VALIDATION__:smoke" in result.message
    assert result.should_force_fail is True


def test_at_least_one_violated_warn_only() -> None:
    cfg = {
        "markers": ["__PULP_VALIDATION__:smoke"],
        "enforce": False,
    }
    result = evaluate_contract(cfg, ())
    assert result.violated is True
    assert result.enforce is False
    # warn-only: do NOT force fail even though the contract is violated
    assert result.should_force_fail is False


# ── require_all mode ────────────────────────────────────────────────────


def test_require_all_satisfied() -> None:
    cfg = {
        "markers": ["__PULP_VALIDATION__:smoke", "__PULP_VALIDATION__:full"],
        "require_at_least_one": False,
    }
    result = evaluate_contract(
        cfg,
        ("__PULP_VALIDATION__:smoke", "__PULP_VALIDATION__:full"),
    )
    assert result.violated is False
    assert result.missing == ()


def test_require_all_one_missing() -> None:
    cfg = {
        "markers": ["__PULP_VALIDATION__:smoke", "__PULP_VALIDATION__:full"],
        "require_at_least_one": False,
    }
    result = evaluate_contract(cfg, ("__PULP_VALIDATION__:smoke",))
    assert result.violated is True
    assert result.missing == ("__PULP_VALIDATION__:full",)
    assert result.message is not None
    assert "every declared marker" in result.message
    assert "__PULP_VALIDATION__:full" in result.message
    assert result.should_force_fail is True


def test_require_all_all_missing() -> None:
    cfg = {
        "markers": ["a", "b", "c"],
        "require_at_least_one": False,
    }
    result = evaluate_contract(cfg, ())
    assert result.violated is True
    assert result.missing == ("a", "b", "c")
    assert result.should_force_fail is True


def test_require_all_warn_only() -> None:
    cfg = {
        "markers": ["a", "b"],
        "require_at_least_one": False,
        "enforce": False,
    }
    result = evaluate_contract(cfg, ("a",))
    assert result.violated is True
    assert result.missing == ("b",)
    assert result.should_force_fail is False  # warn only


# ── Edge cases ──────────────────────────────────────────────────────────


def test_extra_markers_seen_dont_break_validation() -> None:
    """A marker the run emitted but the contract didn't ask for is fine."""
    cfg = {"markers": ["__PULP_VALIDATION__:smoke"]}
    result = evaluate_contract(
        cfg,
        ("__PULP_VALIDATION__:smoke", "__SOMETHING_ELSE__"),
    )
    assert result.violated is False


def test_seen_field_preserves_input() -> None:
    """The evaluation always echoes the seen tuple verbatim."""
    seen = ("a", "b", "c")
    result = evaluate_contract({"markers": ["a"]}, seen)
    assert result.seen == seen


def test_contract_evaluation_is_frozen() -> None:
    """ContractEvaluation should be immutable."""
    result = evaluate_contract(None, ())
    try:
        result.violated = True  # type: ignore[misc]
    except (AttributeError, Exception):
        pass
    else:
        raise AssertionError("ContractEvaluation should be frozen")

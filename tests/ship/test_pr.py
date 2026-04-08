"""Tests for PR operations."""

from __future__ import annotations

import json
import subprocess
from unittest.mock import MagicMock, patch

import pytest

from shipyard.ship.pr import (
    GhError,
    PrInfo,
    create_pr,
    find_pr_for_branch,
    get_pr_status,
    merge_pr,
)


class TestPrInfo:
    def test_to_dict_minimal(self) -> None:
        pr = PrInfo(
            number=42,
            url="https://github.com/org/repo/pull/42",
            title="Add feature",
            state="OPEN",
            branch="feature/x",
            base="main",
        )
        d = pr.to_dict()
        assert d["number"] == 42
        assert d["state"] == "OPEN"
        assert "mergeable" not in d
        assert "checks_passing" not in d

    def test_to_dict_with_optional_fields(self) -> None:
        pr = PrInfo(
            number=10,
            url="https://github.com/org/repo/pull/10",
            title="Fix bug",
            state="OPEN",
            branch="fix/bug",
            base="main",
            mergeable="MERGEABLE",
            checks_passing=True,
        )
        d = pr.to_dict()
        assert d["mergeable"] == "MERGEABLE"
        assert d["checks_passing"] is True

    def test_to_dict_checks_failing(self) -> None:
        pr = PrInfo(
            number=5,
            url="https://github.com/org/repo/pull/5",
            title="WIP",
            state="OPEN",
            branch="wip",
            base="main",
            checks_passing=False,
        )
        d = pr.to_dict()
        assert d["checks_passing"] is False


class TestGetPrStatus:
    @patch("shipyard.ship.pr._run_gh")
    def test_returns_pr_info(self, mock_gh: MagicMock) -> None:
        mock_gh.return_value = json.dumps({
            "number": 42,
            "url": "https://github.com/org/repo/pull/42",
            "title": "Add feature",
            "state": "OPEN",
            "headRefName": "feature/x",
            "baseRefName": "main",
            "mergeable": "MERGEABLE",
            "statusCheckRollup": [
                {"conclusion": "SUCCESS"},
                {"conclusion": "SUCCESS"},
            ],
        })

        pr = get_pr_status("42")
        assert pr.number == 42
        assert pr.checks_passing is True
        assert pr.mergeable == "MERGEABLE"

    @patch("shipyard.ship.pr._run_gh")
    def test_checks_failing(self, mock_gh: MagicMock) -> None:
        mock_gh.return_value = json.dumps({
            "number": 7,
            "url": "https://github.com/org/repo/pull/7",
            "title": "Broken",
            "state": "OPEN",
            "headRefName": "broken",
            "baseRefName": "main",
            "statusCheckRollup": [
                {"conclusion": "SUCCESS"},
                {"conclusion": "FAILURE"},
            ],
        })

        pr = get_pr_status("7")
        assert pr.checks_passing is False

    @patch("shipyard.ship.pr._run_gh")
    def test_no_checks(self, mock_gh: MagicMock) -> None:
        mock_gh.return_value = json.dumps({
            "number": 1,
            "url": "https://github.com/org/repo/pull/1",
            "title": "New",
            "state": "OPEN",
            "headRefName": "new",
            "baseRefName": "main",
            "statusCheckRollup": [],
        })

        pr = get_pr_status("1")
        assert pr.checks_passing is None


class TestFindPrForBranch:
    @patch("shipyard.ship.pr._run_gh")
    def test_finds_existing_pr(self, mock_gh: MagicMock) -> None:
        mock_gh.return_value = json.dumps([{
            "number": 15,
            "url": "https://github.com/org/repo/pull/15",
            "title": "My PR",
            "state": "OPEN",
            "headRefName": "feature/y",
            "baseRefName": "main",
        }])

        pr = find_pr_for_branch("feature/y")
        assert pr is not None
        assert pr.number == 15

    @patch("shipyard.ship.pr._run_gh")
    def test_returns_none_when_no_pr(self, mock_gh: MagicMock) -> None:
        mock_gh.return_value = "[]"

        pr = find_pr_for_branch("no-pr-branch")
        assert pr is None

    @patch("shipyard.ship.pr._run_gh")
    def test_returns_none_on_error(self, mock_gh: MagicMock) -> None:
        mock_gh.side_effect = GhError("not found")

        pr = find_pr_for_branch("error-branch")
        assert pr is None


class TestCreatePr:
    @patch("shipyard.ship.pr.get_pr_status")
    @patch("shipyard.ship.pr._run_gh")
    @patch("shipyard.ship.pr._run_git")
    def test_creates_and_returns_pr(
        self, mock_git: MagicMock, mock_gh: MagicMock, mock_status: MagicMock
    ) -> None:
        mock_git.return_value = ""
        mock_gh.return_value = "https://github.com/org/repo/pull/99\n"
        mock_status.return_value = PrInfo(
            number=99,
            url="https://github.com/org/repo/pull/99",
            title="My feature",
            state="OPEN",
            branch="feature/z",
            base="main",
        )

        pr = create_pr("feature/z", "main", "My feature", "Description here")
        assert pr.number == 99
        mock_git.assert_called_once()


class TestMergePr:
    @patch("shipyard.ship.pr.get_pr_status")
    @patch("shipyard.ship.pr._run_gh")
    def test_merges_pr(self, mock_gh: MagicMock, mock_status: MagicMock) -> None:
        mock_gh.return_value = ""
        mock_status.return_value = PrInfo(
            number=42,
            url="https://github.com/org/repo/pull/42",
            title="Done",
            state="MERGED",
            branch="feature/done",
            base="main",
        )

        pr = merge_pr(42)
        assert pr.state == "MERGED"

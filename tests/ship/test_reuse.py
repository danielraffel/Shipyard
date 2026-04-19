"""Tests for cross-PR evidence reuse (``shipyard.ship.reuse``)."""

from __future__ import annotations

import subprocess
from datetime import datetime, timezone
from typing import TYPE_CHECKING

import pytest

from shipyard.core.evidence import EvidenceRecord, EvidenceStore
from shipyard.ship.reuse import (
    ReuseDecision,
    _fnmatch_recursive,
    _matches_any_glob,
    compute_validation_signature,
    evaluate_reuse,
)

if TYPE_CHECKING:
    from pathlib import Path


# ---- helpers --------------------------------------------------------------


def _git(cmd: list[str], *, cwd: Path) -> str:
    """Run ``git`` in ``cwd`` and return stdout; raise on failure."""
    result = subprocess.run(
        ["git", *cmd], cwd=cwd, capture_output=True, text=True, check=True
    )
    return result.stdout.strip()


@pytest.fixture
def tiny_repo(tmp_path: Path) -> Path:
    """Minimal git repo with three linear commits so tests can exercise
    ancestor ordering and ``git diff --name-only`` without hitting the
    network.

    Commits (oldest → newest):
      1) initial    — ``src/backend/api.py``, ``docs/readme.md``
      2) docs-only  — updates ``docs/readme.md``
      3) docs-only  — updates ``docs/guide.md``
    """
    repo = tmp_path / "repo"
    repo.mkdir()
    _git(["init", "--initial-branch=main"], cwd=repo)
    _git(["config", "user.email", "test@example.com"], cwd=repo)
    _git(["config", "user.name", "Test"], cwd=repo)
    _git(["config", "commit.gpgsign", "false"], cwd=repo)

    (repo / "src").mkdir()
    (repo / "src" / "backend").mkdir()
    (repo / "docs").mkdir()
    (repo / "src" / "backend" / "api.py").write_text("print('hi')\n")
    (repo / "docs" / "readme.md").write_text("initial\n")
    _git(["add", "."], cwd=repo)
    _git(["commit", "-m", "initial"], cwd=repo)

    (repo / "docs" / "readme.md").write_text("updated readme\n")
    _git(["add", "."], cwd=repo)
    _git(["commit", "-m", "docs only 1"], cwd=repo)

    (repo / "docs" / "guide.md").write_text("a guide\n")
    _git(["add", "."], cwd=repo)
    _git(["commit", "-m", "docs only 2"], cwd=repo)

    return repo


def _shas(repo: Path) -> list[str]:
    """Return commit SHAs newest-first (like rev-list)."""
    out = _git(["rev-list", "HEAD"], cwd=repo)
    return [line.strip() for line in out.splitlines() if line.strip()]


def _pass_record(
    sha: str,
    *,
    target: str = "mac",
    contract_digest: str | None = None,
    stages_signature: str | None = None,
) -> EvidenceRecord:
    return EvidenceRecord(
        sha=sha,
        branch="main",
        target_name=target,
        platform="macos-arm64",
        status="pass",
        backend="local",
        completed_at=datetime.now(timezone.utc),
        contract_digest=contract_digest,
        stages_signature=stages_signature,
    )


# ---- glob matching --------------------------------------------------------


class TestGlobMatching:
    def test_simple_prefix(self) -> None:
        assert _matches_any_glob("src/backend/api.py", ["src/backend/**"])

    def test_directory_shortcut(self) -> None:
        assert _matches_any_glob("src/backend/api.py", ["src/backend/"])

    def test_miss(self) -> None:
        assert not _matches_any_glob("docs/readme.md", ["src/backend/**"])

    def test_star_star_leading(self) -> None:
        assert _matches_any_glob("x/y/z/foo.py", ["**/*.py"])

    def test_extension_only(self) -> None:
        assert _matches_any_glob("deep/nested/thing.go", ["**/*.go"])
        assert not _matches_any_glob("deep/nested/thing.py", ["**/*.go"])

    def test_direct_match_no_stars(self) -> None:
        assert _fnmatch_recursive("Makefile", "Makefile")
        assert not _fnmatch_recursive("src/Makefile", "Makefile")


# ---- validation signature -------------------------------------------------


class TestValidationSignature:
    def test_stable_across_dict_order(self) -> None:
        cfg1 = {
            "setup": "./setup.sh",
            "build": "./build.sh",
            "test": "./test.sh",
            "contract": {"markers": ["BUILD_OK"], "enforce": True},
        }
        cfg2 = {
            "contract": {"enforce": True, "markers": ["BUILD_OK"]},
            "test": "./test.sh",
            "build": "./build.sh",
            "setup": "./setup.sh",
        }
        assert compute_validation_signature(cfg1) == compute_validation_signature(cfg2)

    def test_stage_added_changes_signature(self) -> None:
        a = {"build": "b", "test": "t"}
        b = {"build": "b", "test": "t", "configure": "c"}
        assert compute_validation_signature(a)[1] != compute_validation_signature(b)[1]

    def test_contract_change_changes_digest(self) -> None:
        a = {"build": "b", "contract": {"markers": ["X"]}}
        b = {"build": "b", "contract": {"markers": ["Y"]}}
        assert compute_validation_signature(a)[0] != compute_validation_signature(b)[0]


# ---- evaluate_reuse end-to-end -------------------------------------------


class TestEvaluateReuse:
    def test_disabled_by_default(
        self, tiny_repo: Path, evidence_store: EvidenceStore
    ) -> None:
        shas = _shas(tiny_repo)
        head = shas[0]
        decision = evaluate_reuse(
            target_name="mac",
            target_config={},  # no reuse globs -> feature off
            validation_config={"build": "b"},
            head_sha=head,
            evidence_store=evidence_store,
            repo_dir=str(tiny_repo),
        )
        assert decision.reused is False
        assert "reuse not enabled" in decision.reason

    def test_reuses_when_diff_misses_globs(
        self, tiny_repo: Path, evidence_store: EvidenceStore
    ) -> None:
        # Ancestor = commit 1 (initial), HEAD = commit 3. Only docs
        # touched between them, and the target only cares about src/.
        shas = _shas(tiny_repo)
        head_sha = shas[0]
        ancestor_sha = shas[2]
        evidence_store.record(_pass_record(ancestor_sha))
        decision = evaluate_reuse(
            target_name="mac",
            target_config={"reuse_if_paths_unchanged": ["src/backend/**"]},
            validation_config={"build": "b"},
            head_sha=head_sha,
            evidence_store=evidence_store,
            repo_dir=str(tiny_repo),
        )
        assert decision.reused is True
        assert decision.ancestor_sha == ancestor_sha
        assert "reused from" in decision.reason

    def test_refuses_when_diff_touches_globs(
        self, tiny_repo: Path, evidence_store: EvidenceStore
    ) -> None:
        # Create a *new* commit that touches src/backend/api.py and
        # ask for reuse: should refuse.
        shas_before = _shas(tiny_repo)
        ancestor_sha = shas_before[0]  # current HEAD becomes ancestor
        evidence_store.record(_pass_record(ancestor_sha))
        (tiny_repo / "src" / "backend" / "api.py").write_text("print('v2')\n")
        _git(["add", "."], cwd=tiny_repo)
        _git(["commit", "-m", "touch backend"], cwd=tiny_repo)
        head_sha = _git(["rev-parse", "HEAD"], cwd=tiny_repo)

        decision = evaluate_reuse(
            target_name="mac",
            target_config={"reuse_if_paths_unchanged": ["src/backend/**"]},
            validation_config={"build": "b"},
            head_sha=head_sha,
            evidence_store=evidence_store,
            repo_dir=str(tiny_repo),
        )
        assert decision.reused is False
        assert "intersect reuse globs" in decision.reason

    def test_no_passing_ancestor(
        self, tiny_repo: Path, evidence_store: EvidenceStore
    ) -> None:
        shas = _shas(tiny_repo)
        head_sha = shas[0]
        # store has nothing -> no candidate
        decision = evaluate_reuse(
            target_name="mac",
            target_config={"reuse_if_paths_unchanged": ["**"]},
            validation_config={"build": "b"},
            head_sha=head_sha,
            evidence_store=evidence_store,
            repo_dir=str(tiny_repo),
        )
        assert decision.reused is False
        assert "no passing ancestor" in decision.reason

    def test_refuses_on_lineage_break(
        self, tiny_repo: Path, evidence_store: EvidenceStore
    ) -> None:
        # Put a record at a totally unrelated SHA that our first-
        # parent walk would never surface. The query returns None so
        # we just assert "no passing ancestor" — which is the correct
        # refusal for a lineage break under this implementation.
        evidence_store.record(_pass_record("deadbeefcafebabe0123"))
        shas = _shas(tiny_repo)
        decision = evaluate_reuse(
            target_name="mac",
            target_config={"reuse_if_paths_unchanged": ["**"]},
            validation_config={"build": "b"},
            head_sha=shas[0],
            evidence_store=evidence_store,
            repo_dir=str(tiny_repo),
        )
        assert decision.reused is False

    def test_refuses_on_contract_drift(
        self, tiny_repo: Path, evidence_store: EvidenceStore
    ) -> None:
        shas = _shas(tiny_repo)
        head_sha = shas[0]
        ancestor_sha = shas[2]
        # Store a record whose contract digest won't match the current
        # validation config.
        rec = _pass_record(
            ancestor_sha, contract_digest="0000aaaa", stages_signature="build"
        )
        evidence_store.record(rec)
        decision = evaluate_reuse(
            target_name="mac",
            target_config={"reuse_if_paths_unchanged": ["src/backend/**"]},
            validation_config={
                "build": "b",
                "contract": {"markers": ["NEW_MARKER"], "enforce": True},
            },
            head_sha=head_sha,
            evidence_store=evidence_store,
            repo_dir=str(tiny_repo),
        )
        assert decision.reused is False
        assert "contract" in decision.reason

    def test_refuses_on_stage_drift(
        self, tiny_repo: Path, evidence_store: EvidenceStore
    ) -> None:
        shas = _shas(tiny_repo)
        head_sha = shas[0]
        ancestor_sha = shas[2]
        rec = _pass_record(
            ancestor_sha,
            contract_digest=compute_validation_signature({"build": "b"})[0],
            stages_signature="build",  # old config only had build
        )
        evidence_store.record(rec)
        decision = evaluate_reuse(
            target_name="mac",
            target_config={"reuse_if_paths_unchanged": ["src/backend/**"]},
            validation_config={"build": "b", "test": "t"},  # added test
            head_sha=head_sha,
            evidence_store=evidence_store,
            repo_dir=str(tiny_repo),
        )
        assert decision.reused is False
        assert "stage list" in decision.reason


# ---- EvidenceStore.query_passing_for_target -------------------------------


class TestQueryPassingForTarget:
    def test_returns_ranked_match(self, evidence_store: EvidenceStore) -> None:
        for sha in ("a" * 40, "b" * 40, "c" * 40):
            evidence_store.record(EvidenceRecord(
                sha=sha, branch=f"b{sha[0]}", target_name="mac",
                platform="macos", status="pass", backend="local",
                completed_at=datetime.now(timezone.utc),
            ))
        # Candidates ordered b, c, a → should pick b.
        rec = evidence_store.query_passing_for_target(
            "mac", ["b" * 40, "c" * 40, "a" * 40]
        )
        assert rec is not None
        assert rec.sha == "b" * 40

    def test_excludes_reused_records(self, evidence_store: EvidenceStore) -> None:
        evidence_store.record(EvidenceRecord(
            sha="abc", branch="main", target_name="mac",
            platform="macos", status="pass", backend="reused",
            completed_at=datetime.now(timezone.utc),
            reused_from="parent",
        ))
        rec = evidence_store.query_passing_for_target("mac", ["abc"])
        assert rec is None

    def test_excludes_failed(self, evidence_store: EvidenceStore) -> None:
        evidence_store.record(EvidenceRecord(
            sha="abc", branch="main", target_name="mac",
            platform="macos", status="fail", backend="local",
            completed_at=datetime.now(timezone.utc),
        ))
        rec = evidence_store.query_passing_for_target("mac", ["abc"])
        assert rec is None

    def test_returns_none_for_unknown_target(
        self, evidence_store: EvidenceStore
    ) -> None:
        evidence_store.record(EvidenceRecord(
            sha="abc", branch="main", target_name="mac",
            platform="macos", status="pass", backend="local",
            completed_at=datetime.now(timezone.utc),
        ))
        rec = evidence_store.query_passing_for_target("ubuntu", ["abc"])
        assert rec is None


# ---- ReuseDecision dataclass ---------------------------------------------


class TestReuseDecision:
    def test_default_ancestor_fields(self) -> None:
        d = ReuseDecision(reused=False, reason="nope")
        assert d.ancestor_sha is None
        assert d.ancestor_record is None

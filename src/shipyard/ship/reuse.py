"""Cross-PR evidence reuse for rebase-only / non-intersecting PRs.

When a PR's HEAD only adds commits on top of an already-validated
ancestor SHA, and the diff ``<ancestor>..HEAD`` touches no path that a
given target actually exercises, we can *reuse* the ancestor's passing
evidence instead of re-running the target. Opt-in per target via the
``reuse_if_paths_unchanged`` glob list on the target config.

This module holds the pure logic — ancestor check, diff collection,
glob matching, safety gates (contract/stages drift, non-fast-forward) —
plus the git helpers. The CLI's ``_execute_job`` imports
:func:`evaluate_reuse` as a pre-dispatch step per target.

Safety posture:

* Every failure mode returns ``ReuseDecision(reused=False, reason=...)``
  rather than raising. The caller falls back to a normal dispatch;
  reuse is always best-effort.
* We never "chain-reuse" — a candidate record whose ``reused_from`` is
  set is excluded by :meth:`EvidenceStore.query_passing_for_target`.
* Validation contract + stage list changes between SHAs force reuse to
  refuse; see :func:`_validation_signature`.
"""

from __future__ import annotations

import fnmatch
import hashlib
import json
import subprocess
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from shipyard.core.evidence import EvidenceRecord, EvidenceStore


# ---- stage list -------------------------------------------------------
# Source of truth is ``shipyard.executor.local.STAGES`` but importing it
# here would create a cycle (executor imports core, core imports ship).
# Keep a local copy — its shape is stable and changing it would be a
# contract-level bump anyway.
_STAGES = ("setup", "configure", "build", "test")


@dataclass(frozen=True)
class ReuseDecision:
    """Outcome of evaluating reuse for one target.

    ``reused=True`` means the caller should skip dispatch and write a
    synthetic PASS evidence record. ``reused=False`` means fall back
    to the normal dispatch path. ``reason`` is a short human string;
    surfaced in logs and, when reuse succeeds, also in the evidence
    record's ``backend`` field.
    """

    reused: bool
    reason: str
    ancestor_sha: str | None = None
    ancestor_record: EvidenceRecord | None = None
    contract_digest: str | None = None
    stages_signature: str | None = None


def evaluate_reuse(
    *,
    target_name: str,
    target_config: dict[str, Any],
    validation_config: dict[str, Any],
    head_sha: str,
    evidence_store: EvidenceStore,
    repo_dir: str | None = None,
    max_candidates: int = 50,
) -> ReuseDecision:
    """Decide whether to reuse evidence for ``target_name``.

    Parameters mirror what ``_execute_job`` has at dispatch time. The
    function is pure w.r.t. its arguments plus git (via ``repo_dir``)
    and the evidence store — it does not mutate either.

    Returns a :class:`ReuseDecision`. On ``reused=False`` the
    ``reason`` field explains why so operators can see the decision in
    the log tail / ship summary.
    """
    from shipyard.targets import extract_reuse_globs

    globs = extract_reuse_globs(target_config)
    if not globs:
        return ReuseDecision(reused=False, reason="reuse not enabled for target")

    # Walk recent first-parent history for ancestor candidates. We
    # restrict to first-parent so a merge-in of a side branch doesn't
    # accidentally suggest reuse from a commit that was never the
    # trunk's HEAD.
    candidates = _list_ancestor_shas(head_sha, limit=max_candidates, repo_dir=repo_dir)
    if not candidates:
        return ReuseDecision(
            reused=False,
            reason="no ancestor commits found (shallow clone or orphan HEAD)",
        )

    record = evidence_store.query_passing_for_target(target_name, candidates)
    if record is None:
        return ReuseDecision(
            reused=False,
            reason="no passing ancestor evidence for target",
        )

    # Safety: refuse reuse across lineage breaks. ``query_passing``
    # already restricted the SHA to our candidate list, but a shallow
    # clone might have given us a truncated ancestor list. Verify
    # directly.
    if not _is_ancestor(record.sha, head_sha, repo_dir=repo_dir):
        return ReuseDecision(
            reused=False,
            reason=f"ancestor {record.sha[:12]} is not reachable from HEAD",
        )

    # Safety: refuse if validation contract or stage list changed
    # between the ancestor record and current config.
    contract_digest, stages_signature = _validation_signature(validation_config)
    if (
        record.contract_digest is not None
        and record.contract_digest != contract_digest
    ):
        return ReuseDecision(
            reused=False,
            reason="validation contract changed between SHAs",
        )
    if (
        record.stages_signature is not None
        and record.stages_signature != stages_signature
    ):
        return ReuseDecision(
            reused=False,
            reason="validation stage list changed between SHAs",
        )

    # Path intersection. If ANY changed file matches ANY glob, the
    # target exercises the change — reuse would be unsound.
    try:
        changed = _diff_name_only(record.sha, head_sha, repo_dir=repo_dir)
    except _GitError as err:
        return ReuseDecision(
            reused=False,
            reason=f"git diff unavailable: {err}",
        )

    matched = [path for path in changed if _matches_any_glob(path, globs)]
    if matched:
        return ReuseDecision(
            reused=False,
            reason=(
                f"{len(matched)} changed path(s) intersect reuse globs "
                f"(e.g. {matched[0]})"
            ),
        )

    return ReuseDecision(
        reused=True,
        reason=f"reused from {record.sha[:12]} ({len(changed)} file(s) unchanged)",
        ancestor_sha=record.sha,
        ancestor_record=record,
        contract_digest=contract_digest,
        stages_signature=stages_signature,
    )


def compute_validation_signature(
    validation_config: dict[str, Any],
) -> tuple[str, str]:
    """Public wrapper around :func:`_validation_signature`.

    Used by the dispatch site when writing a *real* (non-reuse)
    evidence record so future reuse attempts can compare fingerprints.
    """
    return _validation_signature(validation_config)


def _matches_any_glob(path: str, globs: list[str]) -> bool:
    """True if ``path`` matches any of ``globs``.

    Uses ``fnmatch`` with two fixes for glob-style paths:
      * ``**`` is translated to match across path separators
      * a directory-style glob (e.g. ``src/backend/``) is expanded to
        ``src/backend/**`` so users can declare whole-dir reuse gates
        without remembering the trailing ``**``.
    """
    norm_path = path.replace("\\", "/")
    for raw in globs:
        pattern = raw.replace("\\", "/")
        if pattern.endswith("/"):
            pattern = pattern + "**"
        # fnmatch treats ``*`` as "any chars except /"; expand ``**``
        # to a single-segment ``*`` and rely on a multi-pass match so
        # the user's intuition of "any depth" works.
        if _fnmatch_recursive(norm_path, pattern):
            return True
    return False


def _fnmatch_recursive(path: str, pattern: str) -> bool:
    """``fnmatch`` with recursive ``**`` support.

    ``fnmatch`` doesn't understand ``**`` natively; we split on
    ``/**/`` and require each chunk to match a contiguous path prefix.
    This handles the common cases: ``src/**/*.py``, ``**/*.md``,
    ``docs/**``.
    """
    if "**" not in pattern:
        return fnmatch.fnmatchcase(path, pattern)

    # Normalize leading/trailing ``**`` cases.
    if pattern == "**":
        return True
    if pattern.startswith("**/"):
        tail = pattern[3:]
        # Match ``tail`` at any depth (including zero).
        parts = path.split("/")
        for i in range(len(parts) + 1):
            sub = "/".join(parts[i:])
            if _fnmatch_recursive(sub, tail):
                return True
        return False
    if pattern.endswith("/**"):
        head = pattern[:-3]
        return path == head or path.startswith(head + "/") or fnmatch.fnmatchcase(path, head)

    # General case: a ``/**/`` in the middle.
    head, _, tail = pattern.partition("/**/")
    parts = path.split("/")
    for i in range(1, len(parts) + 1):
        head_candidate = "/".join(parts[:i])
        if fnmatch.fnmatchcase(head_candidate, head):
            rest = "/".join(parts[i:])
            if _fnmatch_recursive(rest, tail):
                return True
    return False


# ---- git helpers ------------------------------------------------------


class _GitError(RuntimeError):
    """Raised when a git invocation fails."""


def _run_git(args: list[str], *, repo_dir: str | None) -> str:
    cmd = ["git"]
    if repo_dir:
        cmd.extend(["-C", repo_dir])
    cmd.extend(args)
    try:
        result = subprocess.run(
            cmd, capture_output=True, text=True, timeout=15, check=False,
        )
    except FileNotFoundError as err:
        raise _GitError("git not installed") from err
    except subprocess.TimeoutExpired as err:
        raise _GitError("git command timed out") from err
    if result.returncode != 0:
        raise _GitError((result.stderr or result.stdout or "git failed").strip())
    return result.stdout


def _list_ancestor_shas(
    head_sha: str, *, limit: int, repo_dir: str | None
) -> list[str]:
    """Return the first-parent ancestor SHAs of HEAD, newest first.

    Includes ``head_sha`` itself (a same-SHA PASS is trivially a
    reuse candidate — handy for re-runs). Returns an empty list on
    any git failure so the caller can fall back gracefully.
    """
    try:
        out = _run_git(
            [
                "rev-list",
                "--first-parent",
                f"--max-count={limit}",
                head_sha,
            ],
            repo_dir=repo_dir,
        )
    except _GitError:
        return []
    return [line.strip() for line in out.splitlines() if line.strip()]


def _is_ancestor(sha: str, head_sha: str, *, repo_dir: str | None) -> bool:
    """True iff ``sha`` is an ancestor of ``head_sha``."""
    try:
        _run_git(
            ["merge-base", "--is-ancestor", sha, head_sha],
            repo_dir=repo_dir,
        )
    except _GitError:
        return False
    return True


def _diff_name_only(
    ancestor_sha: str, head_sha: str, *, repo_dir: str | None
) -> list[str]:
    """Return ``git diff --name-only <ancestor>..HEAD``.

    Empty list when ``ancestor_sha == head_sha``. Raises ``_GitError``
    on any other git failure so the caller refuses reuse rather than
    silently proceeding on a missing diff.
    """
    if ancestor_sha == head_sha:
        return []
    out = _run_git(
        ["diff", "--name-only", f"{ancestor_sha}..{head_sha}"],
        repo_dir=repo_dir,
    )
    return [line.strip() for line in out.splitlines() if line.strip()]


# ---- validation fingerprint ------------------------------------------


def _validation_signature(
    validation_config: dict[str, Any],
) -> tuple[str, str]:
    """Return (contract_digest, stages_signature) for a resolved config.

    The contract digest hashes the ``[validation.contract]`` subtable
    (markers + enforce flag). The stage signature is a
    ``|``-separated list of the stages that *have* commands, in the
    fixed stage order — changing or adding a stage changes the sig.
    Both are stable across Python dict iteration order.
    """
    contract = validation_config.get("contract") or {}
    contract_norm = json.dumps(
        contract, sort_keys=True, default=str, separators=(",", ":")
    )
    contract_digest = hashlib.sha256(contract_norm.encode("utf-8")).hexdigest()[:16]

    present = [s for s in _STAGES if validation_config.get(s)]
    stages_signature = "|".join(present) or "(empty)"
    return contract_digest, stages_signature

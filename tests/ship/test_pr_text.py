"""Tests for PR title + body composition.

These tests drive the pure-logic helpers (no real git, no real PR
creation). Git invocations are mocked via monkeypatching
``subprocess.run``; lane policy is passed in directly rather than
resolved from config.

Coverage checklist (explicit anti-regression for the pulp#616 /
pulp#621 class of bugs):

  * Title never starts with "Ship "
  * Body never contains "Automated by Shipyard"
  * Empty commit message falls back to branch-derived title, NOT
    empty string
  * Body carries commit-body text when present (reviewers get
    context without clicking through to the commit)
  * Advisory-lanes section renders when applicable
"""

from __future__ import annotations

import subprocess
from dataclasses import dataclass, field
from unittest.mock import patch

from shipyard.ship.pr_text import compose_pr_body, compose_pr_title


def _mock_git_run(subject: str = "work", body: str = ""):
    """Return a fake subprocess.run that matches on ``--format=`` arg.

    A non-empty default subject reflects real git: any commit that
    exists returns a subject for ``--format=%s``. Tests that want to
    simulate "git failed / walked past root" should explicitly pass
    ``subject=""`` to match the walker's termination signal.
    """
    def fake(cmd, *args, **kwargs):
        if "--format=%s" in cmd:
            return subprocess.CompletedProcess(cmd, 0, stdout=subject + "\n", stderr="")
        if "--format=%b" in cmd:
            return subprocess.CompletedProcess(cmd, 0, stdout=body + "\n", stderr="")
        return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")
    return fake


def _mock_git_refs(subjects: list[str], bodies: list[str] | None = None):
    """Return a fake subprocess.run that serves different subjects per
    HEAD / HEAD~1 / HEAD~2 ref.

    ``subjects[i]`` is the subject returned for ``HEAD~i`` (or HEAD
    when ``i == 0``). Same shape for ``bodies`` — defaults to empty
    strings. Once the list is exhausted, empty string is returned,
    which the walker treats as "walked past root".
    """
    bodies = bodies if bodies is not None else [""] * len(subjects)

    def _ref_depth(cmd: list[str]) -> int:
        # Cmd looks like: ["git", "log", "-1", "--format=...", "HEAD"]
        # or "HEAD~N". Last element is the ref.
        ref = cmd[-1]
        if ref == "HEAD":
            return 0
        if ref.startswith("HEAD~"):
            return int(ref[len("HEAD~"):])
        return 0

    def fake(cmd, *args, **kwargs):
        depth = _ref_depth(cmd)
        subj = subjects[depth] if depth < len(subjects) else ""
        body = bodies[depth] if depth < len(bodies) else ""
        if "--format=%s" in cmd:
            return subprocess.CompletedProcess(cmd, 0, stdout=subj + "\n", stderr="")
        if "--format=%b" in cmd:
            return subprocess.CompletedProcess(cmd, 0, stdout=body + "\n", stderr="")
        return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")
    return fake


@dataclass
class _FakePolicy:
    advisory_targets: set[str] = field(default_factory=set)
    overrides_from_trailer: set[str] = field(default_factory=set)


# ── compose_pr_title ────────────────────────────────────────────────


def test_title_uses_commit_subject_when_present() -> None:
    with patch("subprocess.run", _mock_git_run(
        subject="fix(cache): stop evicting warm entries on startup",
    )):
        title = compose_pr_title("fix/cache-startup")
    assert title == "fix(cache): stop evicting warm entries on startup"


def test_title_never_prefixes_with_ship() -> None:
    """Regression for the pulp#621 pattern: prior code used
    f'Ship {branch}' and every auto-opened PR title leaked internal
    vocabulary."""
    with patch("subprocess.run", _mock_git_run(
        subject="refactor: extract connection pool",
    )):
        title = compose_pr_title("feature/connection-pool")
    assert not title.startswith("Ship ")
    assert "Ship " not in title


def test_title_falls_back_to_prettified_branch_when_git_empty() -> None:
    with patch("subprocess.run", _mock_git_run(subject="")):
        title = compose_pr_title("feature/foo-bar-baz")
    assert title == "Foo bar baz"
    assert not title.startswith("Ship ")


def test_title_falls_back_when_git_errors() -> None:
    def raise_git(*a, **kw):
        raise subprocess.CalledProcessError(returncode=128, cmd=a)
    with patch("subprocess.run", raise_git):
        title = compose_pr_title("fix/deep-foo")
    assert title == "Deep foo"


# ── compose_pr_body ─────────────────────────────────────────────────


def test_body_uses_commit_body_when_present() -> None:
    commit_body = (
        "The cache's eviction timer fires before the warm pool probe "
        "has a chance to run, so cold-start requests hit an empty cache.\n"
        "\n"
        "Fixes #123."
    )
    with patch("subprocess.run", _mock_git_run(body=commit_body)):
        body = compose_pr_body()
    assert body == commit_body
    assert "Automated by Shipyard" not in body


def test_body_never_contains_shipyard_branding() -> None:
    """Regression for pulp#606 / pulp#616: the body used to trail
    with 'Automated by Shipyard.' which leaks tool identity into
    reviewer-visible text."""
    with patch("subprocess.run", _mock_git_run(
        subject="anything",
        body="a body",
    )):
        body = compose_pr_body()
    assert "Automated by Shipyard" not in body
    assert "Shipyard." not in body
    assert "shipyard pr" not in body


def test_body_empty_string_when_no_commit_body_and_no_advisory() -> None:
    """Regression for pulp#621: an empty lane policy + empty commit
    body used to yield `""` which renders as an unhelpful blank PR
    description. This asserts the *contract* — empty is correct
    when there's nothing to say — but real-world PRs should always
    have a commit body to draw from."""
    with patch("subprocess.run", _mock_git_run(body="")):
        body = compose_pr_body()
    assert body == ""


def test_body_appends_advisory_lanes_section() -> None:
    policy = _FakePolicy(
        advisory_targets={"windows", "freebsd"},
        overrides_from_trailer={"windows"},
    )
    with patch("subprocess.run", _mock_git_run(body="The main reason.")):
        body = compose_pr_body(policy=policy)
    assert "The main reason." in body
    assert "## Advisory lanes" in body
    # Both advisory targets appear, windows with the trailer note.
    assert "`windows` (overridden via Lane-Policy trailer)" in body
    assert "`freebsd`" in body
    # Blank line separates commit body from advisory section.
    assert "The main reason.\n\n## Advisory lanes" in body


def test_body_advisory_section_alone_when_no_commit_body() -> None:
    policy = _FakePolicy(advisory_targets={"windows"})
    with patch("subprocess.run", _mock_git_run(body="")):
        body = compose_pr_body(policy=policy)
    # Starts with the heading — no blank-line-before-content.
    assert body.startswith("## Advisory lanes")
    assert "`windows`" in body


# ── walker: skip mechanical `chore: bump versions` tip ──────────────


def test_title_walks_past_chore_bump_versions_tip() -> None:
    """Regression for the pulp#624 / shipyard#150 observed behavior:
    `shipyard pr` creates a ``chore: bump versions`` commit at the
    tip, and the PR title was reading from THAT commit. The walker
    must skip back to the feature commit."""
    subjects = [
        "chore: bump versions",
        "wait: new shipyard wait primitive — release / pr / run",
    ]
    with patch("subprocess.run", _mock_git_refs(subjects)):
        title = compose_pr_title("feat/wait-primitive")
    assert title == "wait: new shipyard wait primitive — release / pr / run"


def test_body_walks_past_chore_bump_versions_tip() -> None:
    """Same pattern for the body — the feature commit's body
    (the 'why') must surface, not the bump commit's empty body."""
    subjects = [
        "chore: bump versions",
        "wait: new shipyard wait primitive",
    ]
    bodies = [
        "",
        "Adds a daemon-backed wait for release / pr / run conditions.",
    ]
    with patch("subprocess.run", _mock_git_refs(subjects, bodies)):
        body = compose_pr_body()
    assert "daemon-backed wait" in body
    assert "Automated by" not in body
    assert "shipyard pr" not in body


def test_body_never_uses_automated_by_from_bump_commit() -> None:
    """Defense in depth: even if an older shipyard left `Automated by
    shipyard pr.` in a bump commit's body, the walker's subject-based
    skip means we never read from that commit's body at all."""
    subjects = [
        "chore: bump versions",
        "fix: real feature",
    ]
    bodies = [
        "Automated by `shipyard pr`.",
        "Real commit body.",
    ]
    with patch("subprocess.run", _mock_git_refs(subjects, bodies)):
        body = compose_pr_body()
    assert body == "Real commit body."
    assert "Automated by" not in body


def test_title_walks_past_multiple_mechanical_commits() -> None:
    """Walker tolerates a stack of mechanical commits (e.g. a bump
    commit plus a regenerated-changelog commit)."""
    subjects = [
        "chore: bump versions",
        "docs: regenerate changelog for v0.23.0 [skip ci]",
        "feat: the real work",
    ]
    with patch("subprocess.run", _mock_git_refs(subjects)):
        title = compose_pr_title("feat/work")
    assert title == "feat: the real work"


def test_title_falls_back_to_branch_when_every_commit_is_mechanical() -> None:
    """Degenerate case — if every commit in the walk window is a
    mechanical bump, degrade to the branch-name fallback rather than
    leaking the bump subject as the title."""
    subjects = ["chore: bump versions"] * 3 + [""]  # walked past root
    with patch("subprocess.run", _mock_git_refs(subjects)):
        title = compose_pr_title("feature/new-thing")
    assert title == "New thing"

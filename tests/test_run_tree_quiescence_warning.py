"""Tests for #238: `shipyard run` warns that the working tree must
stay quiescent during the run.

The warning is a cheap guard against a real failure mode — a
multi-agent flow (human + agent editing in the same tree, or two
agents in the same worktree) can race mid-run and produce
non-deterministic mid-stage failures (e.g. cmake "Cannot find
source file"). Full solution would be a sandboxed run tree or
fail-fast tree-drift detection; for P3 we ship the discoverability
fix and document the workaround.

Regression guards:
  - `shipyard run --help` surfaces the warning in the docstring
  - issue # is referenced so someone reading the code can find the
    discussion
"""

from __future__ import annotations

import sys

import pytest  # noqa: TC002 — MonkeyPatch fixture usage
from click.testing import CliRunner

from shipyard.cli import main

pytestmark = pytest.mark.skipif(
    sys.platform == "win32",
    reason=(
        "#198: Click CliRunner isolation flake on Windows across "
        "this family of CLI tests. Coverage preserved on Linux + macOS."
    ),
)


def test_run_help_includes_tree_quiescence_warning() -> None:
    # The docstring is what `shipyard run --help` renders. Users
    # who read --help before running should see the caveat.
    runner = CliRunner()
    result = runner.invoke(main, ["run", "--help"])
    assert result.exit_code == 0
    lowered = result.output.lower()
    # Key phrase — don't over-constrain the wording. Must mention
    # concurrent edits / working tree as the failure mode.
    assert "edit" in lowered
    assert "working tree" in lowered or "tracked source" in lowered \
        or "source files" in lowered
    # Issue number referenced so a reader can find the discussion.
    assert "#238" in result.output


def test_run_docstring_mentions_concrete_failure_shape() -> None:
    # Keep the "Cannot find source file" breadcrumb in the docstring
    # so anyone grepping the failure they hit lands on this context.
    # If the test fails because someone cleaned up the docstring,
    # they should leave at least a breadcrumb explaining what users
    # would search for.
    from shipyard.cli import run as run_cmd
    doc = (run_cmd.__doc__ or "").lower()
    assert "concurrent" in doc or "quiescent" in doc or "edit" in doc
    assert "cmake" in doc or "configure" in doc or "source" in doc

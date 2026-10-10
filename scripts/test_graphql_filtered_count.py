"""No query may read `totalCount` from an `itemTypes`-filtered timeline.

GitHub's `timelineItems(itemTypes:[...])` connection filters its `nodes` and
`filteredCount`, but its `totalCount` ignores `itemTypes` and counts EVERY
timeline item on the pull request. `adoption_audit.sh` once read
`timelineItems(itemTypes:[AUTO_MERGE_ENABLED_EVENT]){totalCount}` and so
reported every merged PR as auto-merged. Read the filtered `nodes` (checking
`__typename`) or `filteredCount` instead.

This module also runs the offline adoption-audit suite, which exercises that
query against GitHub's real response shape, so it executes in CI.
"""

from __future__ import annotations

import pathlib
import re
import shutil
import subprocess
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
SUFFIXES = {".rs", ".py", ".sh", ".bash", ".yml", ".yaml", ".toml", ".js",
            ".mjs", ".ts", ".json"}
SELF = "scripts/test_graphql_filtered_count.py"

_TIMELINE = re.compile(r"timelineItems\s*\(")
_TOTAL = re.compile(r"\btotalCount\b")


def _args_end(text: str, start: int) -> int | None:
    depth = 0
    for i in range(start, len(text)):
        if text[i] == "(":
            depth += 1
        elif text[i] == ")":
            depth -= 1
            if depth == 0:
                return i + 1
    return None


def _direct_selection(text: str, start: int) -> str | None:
    """The connection's own fields: the selection set after `start` with nested
    selections removed. A doubled `{{` opener (a Rust format string) counts as
    one level."""
    i = text.find("{", start)
    if i < 0:
        return None
    width = 1
    while i + width < len(text) and text[i + width] == "{":
        width += 1
    depth, out, j = width, [], i + width
    while j < len(text) and depth > 0:
        ch = text[j]
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
        elif depth == width:
            out.append(ch)
        j += 1
    return "".join(out)


def find_offences(text: str) -> tuple[int, list[int]]:
    """`(filtered_selections_seen, offending_line_numbers)`."""
    seen, offences = 0, []
    for m in _TIMELINE.finditer(text):
        end = _args_end(text, m.end() - 1)
        if end is None or "itemTypes" not in text[m.end():end]:
            continue
        seen += 1
        selection = _direct_selection(text, end)
        if selection is not None and _TOTAL.search(selection):
            offences.append(text.count("\n", 0, m.start()) + 1)
    return seen, offences


def scan_repo() -> tuple[int, list[str]]:
    files = subprocess.run(["git", "-C", str(ROOT), "ls-files"],
                           capture_output=True, text=True,
                           check=True).stdout.splitlines()
    seen, problems = 0, []
    for rel in files:
        if rel == SELF or pathlib.Path(rel).suffix not in SUFFIXES:
            continue
        try:
            text = (ROOT / rel).read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        n, lines = find_offences(text)
        seen += n
        problems += [f"{rel}:{line}" for line in lines]
    return seen, problems


class FindOffencesTest(unittest.TestCase):
    def test_filtered_total_count_is_flagged(self) -> None:
        text = ('q="pullRequest(number:$n){timelineItems(itemTypes:'
                '[AUTO_MERGE_ENABLED_EVENT],first:1){totalCount}}"')
        self.assertEqual(find_offences(text), (1, [1]))

    def test_rust_format_string_is_flagged(self) -> None:
        text = ('format!("{{timelineItems(last:1,itemTypes:[X]){{'
                'totalCount nodes{{__typename}}}}}}")')
        self.assertEqual(find_offences(text), (1, [1]))

    def test_nodes_filtered_count_and_nested_counts_pass(self) -> None:
        text = ('timelineItems(first:1,itemTypes:[X]){filteredCount '
                'nodes{... on PullRequestCommit{commit{parents{totalCount}}}}} '
                'commits{totalCount}')
        self.assertEqual(find_offences(text), (1, []))

    def test_unfiltered_total_count_is_allowed(self) -> None:
        self.assertEqual(find_offences("timelineItems(first:1){totalCount}"),
                         (0, []))


class RepoScanTest(unittest.TestCase):
    def test_no_tracked_query_reads_a_filtered_total_count(self) -> None:
        seen, problems = scan_repo()
        self.assertEqual(problems, [])
        # Control: the merge-queue and auto-merge readers are filtered timeline
        # selections, so a scan that saw fewer reached nothing.
        self.assertGreaterEqual(seen, 8)


@unittest.skipUnless(shutil.which("bash") and shutil.which("jq"),
                     "the adoption-audit suite needs bash and jq")
class AdoptionAuditSuiteTest(unittest.TestCase):
    def test_offline_suite_passes(self) -> None:
        res = subprocess.run(
            ["bash", str(ROOT / "skills/ci/scripts/test_adoption_audit.sh")],
            capture_output=True, text=True)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        self.assertIn(" 0 failed", res.stdout)


if __name__ == "__main__":
    unittest.main()

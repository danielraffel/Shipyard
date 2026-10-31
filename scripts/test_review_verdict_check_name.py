#!/usr/bin/env python3
"""Only the canonical workflow may produce a check or status named `review-verdict`.

The merge steward merges a no-queue base on the strength of a required
`review-verdict` check. GitHub matches a required check by name (and at best by
app), so a second workflow that defines a job of that name, or posts a status
with that context, could satisfy it for a head nobody approved.
"""
from __future__ import annotations

import re
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github" / "workflows"
CANONICAL = "review-verdict.yml"
NAME = "review-verdict"

# A status context or check name spelled out anywhere in a workflow's text,
# e.g. `-f context=review-verdict` or `"context": "review-verdict"`.
STATUS_CONTEXT = re.compile(r"""(context|check[_-]?name)["']?\s*[:=]\s*["']?review-verdict\b""", re.I)


def _is_name(value: object) -> bool:
    return isinstance(value, str) and value.strip().lower() == NAME


def violations(path: Path) -> list[str]:
    """Every way `path` defines a job, check, or status named `review-verdict`."""
    text = path.read_text(encoding="utf-8")
    found = []
    workflow = yaml.safe_load(text) or {}
    for job_id, job in (workflow.get("jobs") or {}).items():
        if _is_name(job_id):
            found.append(f"{path.name}: job id `{job_id}`")
        if isinstance(job, dict) and _is_name(job.get("name")):
            found.append(f"{path.name}: job `{job_id}` is named `{job['name']}`")
    for line_number, line in enumerate(text.splitlines(), start=1):
        if STATUS_CONTEXT.search(line):
            found.append(f"{path.name}:{line_number}: status/check context `{NAME}`")
    return found


def workflow_files() -> list[Path]:
    return sorted([*WORKFLOWS.glob("*.yml"), *WORKFLOWS.glob("*.yaml")])


class ReviewVerdictCheckNameTests(unittest.TestCase):
    def test_no_other_workflow_defines_a_review_verdict_check(self) -> None:
        offenders = [
            violation
            for path in workflow_files()
            if path.name != CANONICAL
            for violation in violations(path)
        ]
        self.assertEqual(offenders, [], "only review-verdict.yml may define it")

    def test_the_canonical_workflow_still_defines_it(self) -> None:
        # Control: the lint must see the one real definition, or it is blind.
        self.assertTrue(violations(WORKFLOWS / CANONICAL))

    def test_the_lint_catches_each_spelling(self) -> None:
        import tempfile

        samples = {
            "job-id.yml": "jobs:\n  review-verdict:\n    runs-on: x\n",
            "job-name.yml": "jobs:\n  x:\n    name: Review-Verdict\n    runs-on: x\n",
            "status.yml": "jobs:\n  x:\n    runs-on: x\n    steps:\n      - run: gh api statuses -f context=review-verdict\n",
        }
        with tempfile.TemporaryDirectory() as directory:
            for name, body in samples.items():
                path = Path(directory) / name
                path.write_text(body, encoding="utf-8")
                self.assertTrue(violations(path), name)
            clean = Path(directory) / "clean.yml"
            clean.write_text("jobs:\n  lint:\n    runs-on: x\n", encoding="utf-8")
            self.assertEqual(violations(clean), [])


if __name__ == "__main__":
    unittest.main()

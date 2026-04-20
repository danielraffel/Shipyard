#!/usr/bin/env python3
"""Doc-sync gate.

Mirrors `skill_sync_check.py` but for free-form docs. Given a diff
range (base..head), check that every doc whose mapped paths were
touched also has the doc itself updated in the same range — or has a
`Doc-Update: skip doc=<path> reason="..."` trailer on the tip commit.

Map lives at `scripts/doc_sync_map.json`. Initially maps
`docs/ship-state-machine.md` to `src/shipyard/core/ship_state.py`
and `src/shipyard/ship/**`, per #101 Phase C.

Modes
    report — hard-fail (exit 1) on any finding. CI uses this.
    hint   — advisory text only (exit 0). Agent hooks use this.
    apply  — alias for report (no mutations possible here; the script
             can't write a doc for you).

Exit codes
    0 — no unresolved findings
    1 — at least one doc needs updating or a bypass
    2 — internal error (bad config, git failure)
"""

from __future__ import annotations

import argparse
import fnmatch
import json
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable


# ── Types ───────────────────────────────────────────────────────────────


@dataclass
class DocEntry:
    doc: str
    description: str
    paths: list[str]


@dataclass
class DocMap:
    docs: list[DocEntry]


@dataclass
class Finding:
    doc: str
    touched_paths: list[str]
    doc_modified: bool
    bypass_reason: str | None = None


# ── Git helpers ────────────────────────────────────────────────────────


def repo_root() -> Path:
    out = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        check=True, capture_output=True, text=True,
    )
    return Path(out.stdout.strip())


def git_diff_names(base: str, head: str) -> list[str]:
    out = subprocess.run(
        ["git", "diff", "--name-only", f"{base}...{head}"],
        check=True, capture_output=True, text=True,
    )
    return [line for line in out.stdout.splitlines() if line]


def git_range_trailers(base: str, head: str) -> dict[str, list[str]]:
    """Collect trailers from every commit in `base..head`, merged.

    Mirrors the skill_sync_check approach so a bypass on ANY commit in
    the range honors — matches pre-push and CI reality where the tip
    commit may not be the authored one.
    """
    commits = subprocess.run(
        ["git", "rev-list", f"{base}..{head}"],
        check=True, capture_output=True, text=True,
    ).stdout.splitlines()

    merged: dict[str, list[str]] = {}
    for sha in commits:
        if not sha:
            continue
        msg = subprocess.run(
            ["git", "log", "-1", "--pretty=%B", sha],
            check=True, capture_output=True, text=True,
        ).stdout
        trailers = subprocess.run(
            ["git", "interpret-trailers", "--parse"],
            input=msg, check=True, capture_output=True, text=True,
        )
        for line in trailers.stdout.splitlines():
            if ":" not in line:
                continue
            key, _, value = line.partition(":")
            merged.setdefault(key.strip().lower(), []).append(value.strip())
    return merged


# ── Config loading ─────────────────────────────────────────────────────


def _strip_comments(data: dict) -> dict:
    return {k: v for k, v in data.items() if not k.startswith("_")}


def load_doc_map(path: Path) -> DocMap:
    raw = _strip_comments(json.loads(path.read_text()))
    docs_raw = raw.get("docs") or {}
    if not isinstance(docs_raw, dict):
        raise ValueError(f"{path}: 'docs' must be an object")
    entries: list[DocEntry] = []
    for doc_path, spec in docs_raw.items():
        if not isinstance(spec, dict):
            raise ValueError(f"{path}: docs.{doc_path} must be an object")
        paths = spec.get("paths") or []
        if not isinstance(paths, list) or not all(isinstance(p, str) for p in paths):
            raise ValueError(f"{path}: docs.{doc_path}.paths must be a list of strings")
        entries.append(
            DocEntry(
                doc=doc_path,
                description=str(spec.get("description", "")),
                paths=list(paths),
            )
        )
    return DocMap(docs=entries)


# ── Matching ───────────────────────────────────────────────────────────


def _matches_any(path: str, patterns: Iterable[str]) -> bool:
    for pattern in patterns:
        # Match absolute glob OR the `**` recursive pattern.
        if fnmatch.fnmatch(path, pattern):
            return True
        # fnmatch doesn't know about `**`; fall back to matching against
        # the pattern minus trailing `/**`.
        if pattern.endswith("/**") and (
            path == pattern[:-3] or path.startswith(pattern[:-2])
        ):
            return True
    return False


def _parse_skip_trailer(trailer_values: list[str]) -> dict[str, str]:
    """Parse `Doc-Update: skip doc=<path> reason="..."` entries."""
    skips: dict[str, str] = {}
    for value in trailer_values:
        if not value.lower().startswith("skip"):
            continue
        # Match `doc=<path>` — path may contain `/` and `.`; reason may
        # include spaces if quoted. Keep the parse forgiving.
        doc: str | None = None
        reason = ""
        for part in value.split():
            if part.startswith("doc="):
                doc = part.partition("=")[2]
        low = value.lower()
        if "reason=" in low:
            # Extract everything inside the first set of quotes after reason=.
            idx = low.find('reason="')
            if idx >= 0:
                tail = value[idx + len('reason="'):]
                end = tail.find('"')
                if end >= 0:
                    reason = tail[:end]
        if doc:
            skips[doc] = reason or "(no reason given)"
    return skips


# ── Core check ─────────────────────────────────────────────────────────


def find_violations(
    doc_map: DocMap,
    changed: list[str],
    trailers: dict[str, list[str]],
    trailer_key: str = "doc-update",
) -> list[Finding]:
    skips = _parse_skip_trailer(trailers.get(trailer_key, []))

    findings: list[Finding] = []
    for entry in doc_map.docs:
        touched = [p for p in changed if _matches_any(p, entry.paths)]
        if not touched:
            continue
        doc_modified = entry.doc in changed
        bypass = skips.get(entry.doc)
        findings.append(
            Finding(
                doc=entry.doc,
                touched_paths=touched,
                doc_modified=doc_modified,
                bypass_reason=bypass,
            )
        )
    return findings


def format_findings(findings: list[Finding]) -> tuple[str, bool]:
    """Render a human-readable report + return whether any are unresolved."""
    lines: list[str] = []
    unresolved = False
    for f in findings:
        if f.doc_modified:
            lines.append(f"✓ {f.doc}: touched + doc updated in the same diff.")
            continue
        if f.bypass_reason is not None:
            lines.append(
                f"↷ {f.doc}: touched, doc not updated — bypassed via "
                f'Doc-Update: skip ("{f.bypass_reason}").'
            )
            continue
        unresolved = True
        lines.append(
            f"✗ {f.doc}: mapped paths changed without updating the doc."
        )
        for path in f.touched_paths:
            lines.append(f"    - {path}")
        lines.append(
            "  Fix: update the doc in the same diff, or add "
            f'`Doc-Update: skip doc={f.doc} reason="..."` on the tip commit.'
        )
    return "\n".join(lines), unresolved


# ── Main ───────────────────────────────────────────────────────────────


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Doc-sync gate")
    parser.add_argument("--base", default="origin/main")
    parser.add_argument("--head", default="HEAD")
    parser.add_argument(
        "--map",
        default=None,
        help="Path to doc_sync_map.json (defaults to scripts/doc_sync_map.json).",
    )
    parser.add_argument("--mode", choices=("report", "hint", "apply"), default="report")
    parser.add_argument("--repo-root", default=None)
    args = parser.parse_args(argv)

    try:
        root = Path(args.repo_root) if args.repo_root else repo_root()
        map_path = (
            Path(args.map)
            if args.map
            else root / "scripts" / "doc_sync_map.json"
        )
        if not map_path.exists():
            sys.stderr.write(
                f"doc_sync_check: map not found: {map_path}\n"
            )
            return 2
        doc_map = load_doc_map(map_path)
        changed = git_diff_names(args.base, args.head)
        trailers = git_range_trailers(args.base, args.head)
    except (subprocess.CalledProcessError, ValueError) as exc:
        sys.stderr.write(f"doc_sync_check: {exc}\n")
        return 2

    findings = find_violations(doc_map, changed, trailers)
    if not findings:
        print("doc-sync: no mapped paths touched — nothing to verify.")
        return 0

    report, unresolved = format_findings(findings)
    print(report)

    if args.mode == "hint":
        return 0
    return 1 if unresolved else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

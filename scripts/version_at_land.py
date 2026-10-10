#!/usr/bin/env python3
"""Assign Shipyard versions after merge.

The PR gate deliberately owns no version counter. This command is the single
writer that runs on ``main`` after a merge, computes the same semver level as
the gate, updates every file in the surface, and commits a marker that makes
the operation idempotent. A failed non-fast-forward push is retried from the
new main tip, so two adjacent merges cannot overwrite one another.
"""

from __future__ import annotations

import argparse
import json
import subprocess
from dataclasses import dataclass
from pathlib import Path

from version_bump_check import (
    LEVELS,
    Config,
    assess_surfaces,
    bump_version,
    git_diff_names,
    load_config,
    read_version,
    refresh_cargo_lock,
    version_at_base,
    write_version,
)

MARKER = "Version-Bump-Applied"


@dataclass(frozen=True)
class Assignment:
    surface: str
    level: str
    current: str
    assigned: str


def git(repo: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["git", "-C", str(repo), *args], check=check,
                          capture_output=True, text=True)


def marker(repo: Path, head: str) -> str | None:
    result = git(repo, "log", "-E", "--grep", rf"^{MARKER}:", "-1",
                 "--format=%H", head, check=False)
    value = result.stdout.strip()
    return value or None


def drain_base(repo: Path, head: str, fallback: str | None) -> str:
    found = marker(repo, head)
    if found:
        return found
    if fallback and git(repo, "merge-base", "--is-ancestor", fallback, head,
                        check=False).returncode == 0:
        return fallback
    return git(repo, "rev-parse", f"{head}^1").stdout.strip()


def plan(repo: Path, config: Config, base: str, head: str) -> list[Assignment]:
    changed = git_diff_names(base, head)
    verdicts = assess_surfaces(config, changed, base, head, repo)
    assignments: list[Assignment] = []
    for verdict in verdicts:
        if verdict.final_level == "none":
            continue
        current = next((version_at_base(base, vf)
                        for vf in verdict.surface.version_files
                        if version_at_base(base, vf)), None)
        if not current:
            current = next((read_version(repo, vf)
                            for vf in verdict.surface.version_files
                            if read_version(repo, vf)), None)
        if current:
            assignments.append(Assignment(
                verdict.surface.name, verdict.final_level, current,
                bump_version(current, verdict.final_level)))
    return assignments


def apply(repo: Path, config: Config, assignments: list[Assignment]) -> list[str]:
    by_name = {surface.name: surface for surface in config.surfaces}
    edited: list[str] = []
    for assignment in assignments:
        for vf in by_name[assignment.surface].version_files:
            if write_version(repo, vf, assignment.assigned):
                edited.append(vf.path)
                if vf.path == "Cargo.toml":
                    lock = refresh_cargo_lock(repo, vf.path, assignment.assigned)
                    if lock:
                        edited.append(lock)
    return list(dict.fromkeys(edited))


def message(assignments: list[Assignment], base: str, head: str) -> str:
    summary = "; ".join(
        f"{a.surface} {a.current}->{a.assigned} ({a.level})" for a in assignments
    )
    return ("chore: assign versions at land\n\n"
            f"Assigned from merged range {base[:12]}..{head[:12]}.\n"
            f"{summary}\n\n{MARKER}: {head}\n")


def run(repo: Path, config: Config, remote: str, branch: str,
        fallback: str | None, retries: int, push: bool) -> tuple[str, list[Assignment]]:
    for attempt in range(retries + 1):
        if push:
            git(repo, "fetch", "--quiet", remote, branch)
            head = git(repo, "rev-parse", f"{remote}/{branch}").stdout.strip()
            git(repo, "reset", "--hard", head)
        else:
            head = "HEAD"
        base = drain_base(repo, head, fallback)
        assignments = plan(repo, config, base, head)
        if not assignments:
            return "noop", []
        if not push:
            return "dry-run", assignments
        edited = apply(repo, config, assignments)
        if not edited:
            return "noop", assignments
        git(repo, "add", "--", *edited)
        git(repo, "commit", "--no-verify", "-m", message(assignments, base, head))
        result = git(repo, "push", "--porcelain", remote, f"HEAD:{branch}", check=False)
        if result.returncode == 0:
            return "applied", assignments
        print(result.stderr or result.stdout, flush=True)
        git(repo, "reset", "--hard", head)
    return "exhausted", []


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base")
    parser.add_argument("--head", default="HEAD")
    parser.add_argument("--config", default="scripts/versioning.json")
    parser.add_argument("--push", action="store_true")
    parser.add_argument("--remote", default="origin")
    parser.add_argument("--branch", default="main")
    parser.add_argument("--max-retries", type=int, default=3)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    root = Path(git(Path.cwd(), "rev-parse", "--show-toplevel").stdout.strip())
    config = load_config((root / args.config).resolve())
    if not args.push and not args.base:
        parser.error("--base is required unless --push is given")
    status, assignments = run(root, config, args.remote, args.branch, args.base,
                              args.max_retries, args.push)
    payload = {"status": status, "plan": [a.__dict__ for a in assignments]}
    if args.json:
        print(json.dumps(payload, indent=2))
    elif not assignments:
        print("version-at-land: no release-worthy changes")
    else:
        for assignment in assignments:
            print(f"version-at-land: {assignment.surface} {assignment.current} -> "
                  f"{assignment.assigned} ({assignment.level})")
    return 0 if status != "exhausted" else 1


if __name__ == "__main__":
    raise SystemExit(main())

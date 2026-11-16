#!/usr/bin/env python3
"""Print the base the versioning gates diff a change against.

A `pull_request` run checks out GitHub's merge of the head into the base,
so `origin/<base>` is exact. Any other run (a `workflow_dispatch` on a
branch, which App-opened pull requests need) checks out the bare branch
head. Diffed two-dot against `origin/main`, that head reports every commit
main gained since the branch was cut as the branch's own change: once
version-at-land bumps `Cargo.toml` on main, every such run fails with
"Version files are owned by version-at-land". The base for those runs is
where the branch left main.

usage: resolve_diff_base.py --event EVENT [--base-ref REF] [--default-branch main]
"""
from __future__ import annotations

import argparse
import subprocess
import sys


def resolve(event: str, base_ref: str, default_branch: str, cwd: str | None = None) -> str:
    if event == "pull_request":
        return f"origin/{base_ref or default_branch}"
    result = subprocess.run(
        ["git", "merge-base", f"origin/{default_branch}", "HEAD"],
        cwd=cwd,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0 or not result.stdout.strip():
        raise SystemExit(
            f"cannot find where HEAD left origin/{default_branch}: {result.stderr.strip()}"
        )
    return result.stdout.strip()


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--event", required=True)
    parser.add_argument("--base-ref", default="")
    parser.add_argument("--default-branch", default="main")
    args = parser.parse_args(argv)
    print(resolve(args.event, args.base_ref, args.default_branch))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

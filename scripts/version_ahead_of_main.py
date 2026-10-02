#!/usr/bin/env python3
"""Require a pull request's CLI version to be strictly ahead of live main.

The version-bump gate checks that a PR moved `Cargo.toml`'s version relative
to its own merge base. Two PRs cut from the same main both pass that check
with the same next version, and Shipyard's main has no merge queue or
up-to-date rule, so both can merge: #677 and #680 both landed as 0.245.0 and
the second shipped untagged. This compares against main as it is now.

    check  --head-ref REF --main-ref REF
        Exit 1 unless the version at REF is strictly greater than main's.
        Run on every pull request.
    sweep  --repo OWNER/REPO --main-ref REF
        After main moves, re-judge every open PR and post the commit status
        `shipyard/version-ahead-of-main` on its head, so a PR that was green
        before another one merged turns red instead of merging a duplicate.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from typing import Callable

STATUS_CONTEXT = "shipyard/version-ahead-of-main"
VERSION_RE = re.compile(r'(?ms)^\[package\]\s*$.*?^version\s*=\s*"([^"]+)"')


def cargo_version(text: str) -> str | None:
    """The `[package]` version in a Cargo.toml, or None."""
    match = VERSION_RE.search(text)
    return match.group(1) if match else None


def parse_semver(value: str) -> tuple[int, int, int] | None:
    match = re.fullmatch(r"v?(\d+)\.(\d+)\.(\d+)", value.strip())
    if match is None:
        return None
    major, minor, patch = (int(part) for part in match.groups())
    return major, minor, patch


def verdict(pr_version: str | None, main_version: str | None) -> tuple[bool, str]:
    """Whether a PR at `pr_version` may merge onto main at `main_version`."""
    pr = parse_semver(pr_version or "")
    main = parse_semver(main_version or "")
    if pr is None or main is None:
        return False, f"unreadable version (PR {pr_version!r}, main {main_version!r})"
    if pr > main:
        return True, f"{pr_version} is ahead of main {main_version}"
    return False, (
        f"{pr_version} is not ahead of main {main_version}: merge main and bump "
        "past it, or this PR would ship a duplicate, untagged version"
    )


def _git_show(ref: str, path: str = "Cargo.toml") -> str:
    return subprocess.run(
        ["git", "show", f"{ref}:{path}"], check=True, capture_output=True, text=True
    ).stdout


def command_check(args: argparse.Namespace) -> int:
    ok, reason = verdict(
        cargo_version(_git_show(args.head_ref)), cargo_version(_git_show(args.main_ref))
    )
    print(f"version-ahead-of-main: {reason}", file=sys.stdout if ok else sys.stderr)
    return 0 if ok else 1


def sweep(
    main_version: str | None,
    open_pulls: list[dict],
    head_cargo: Callable[[str], str | None],
    post_status: Callable[[str, str, str], None],
) -> list[tuple[int, bool, str]]:
    """Judge every open PR against `main_version` and post its status."""
    results = []
    for pull in open_pulls:
        sha = pull["head"]["sha"]
        text = head_cargo(sha)
        ok, reason = verdict(cargo_version(text) if text else None, main_version)
        post_status(sha, "success" if ok else "failure", reason)
        results.append((pull["number"], ok, reason))
    return results


def _gh_json(args: list[str]) -> object:
    output = subprocess.run(
        ["gh", "api", *args], check=True, capture_output=True, text=True
    ).stdout
    return json.loads(output)


def command_sweep(args: argparse.Namespace) -> int:
    pulls = _gh_json([f"repos/{args.repo}/pulls?state=open&base=main&per_page=100"])
    assert isinstance(pulls, list)
    same_repo = [
        pull for pull in pulls if pull["head"]["repo"] and pull["head"]["repo"]["full_name"] == args.repo
    ]

    def head_cargo(sha: str) -> str | None:
        result = subprocess.run(
            ["gh", "api", "-H", "Accept: application/vnd.github.raw",
             f"repos/{args.repo}/contents/Cargo.toml?ref={sha}"],
            capture_output=True, text=True, check=False,
        )
        return result.stdout if result.returncode == 0 else None

    def post_status(sha: str, state: str, description: str) -> None:
        subprocess.run(
            ["gh", "api", "-X", "POST", f"repos/{args.repo}/statuses/{sha}",
             "-f", f"state={state}", "-f", f"context={STATUS_CONTEXT}",
             "-f", f"description={description[:140]}"],
            check=True, capture_output=True, text=True,
        )

    results = sweep(cargo_version(_git_show(args.main_ref)), same_repo, head_cargo, post_status)
    for number, ok, reason in results:
        print(f"#{number}: {'ok' if ok else 'BEHIND'}: {reason}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    commands = parser.add_subparsers(dest="command", required=True)
    check = commands.add_parser("check")
    check.add_argument("--head-ref", required=True)
    check.add_argument("--main-ref", default="origin/main")
    check.set_defaults(func=command_check)
    sweep_parser = commands.add_parser("sweep")
    sweep_parser.add_argument("--repo", required=True)
    sweep_parser.add_argument("--main-ref", default="origin/main")
    sweep_parser.set_defaults(func=command_sweep)
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())

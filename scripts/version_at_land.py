#!/usr/bin/env python3
"""Assign Shipyard versions after merge.

The PR gate deliberately owns no version counter. This command is the single
writer that runs on ``main`` after a merge, computes the same semver level as
the gate, updates every file in the surface, and commits a marker that makes
the operation idempotent.

Main's ruleset requires checks that only a pull request can carry, so the
writer never pushes to main. With ``--pr-branch`` it rebuilds one bot-owned
branch from the current main tip with a single bump commit and opens, or
updates, the one pull request for that branch. Every landing re-plans from the
last bump that reached main, so the pull request always carries the cumulative
bump and never stacks one bump on another. The pull request is handed to the
merge steward (managed label plus an exact-head handoff status); it still needs
green required checks and a reviewer's exact-head approval to land.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
from dataclasses import dataclass
from pathlib import Path

from version_bump_check import (
    Config,
    assess_surfaces,
    bump_version,
    git_diff_names,
    load_config,
    max_level,
    read_version,
    refresh_cargo_lock,
    version_at_base,
    write_version,
)

MARKER = "Version-Bump-Applied"
# The bump commit edits version files, which the PR gate refuses without this
# explicit release trailer.
RELEASE_TRAILER = 'Release: allow-version-files reason="version-at-land bump pull request"'
SYNC_REASON = "version-at-land bump: version files only"
MANAGED_LABEL = "shipyard:managed"
HANDOFF_CONTEXT = "shipyard/steward-handoff"
HANDOFF_DESCRIPTION = "Managed handoff version-at-land"


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
    # --no-merges: the merge of the bump pull request repeats its title, and
    # only the bump commit itself names the main tip it covered.
    result = git(repo, "log", "--no-merges", "-E", "--grep", rf"^{MARKER}:", "-1",
                 "--format=%H", head, check=False)
    value = result.stdout.strip()
    return value or None


def is_marker_commit(repo: Path, commit: str) -> bool:
    body = git(repo, "log", "-1", "--format=%B", commit).stdout
    return any(line.startswith(f"{MARKER}:") for line in body.splitlines())


def is_bump_landing(repo: Path, commit: str) -> bool:
    """Whether a first-parent commit on main is a bump reaching main.

    Either the bump commit itself (the legacy direct push) or the merge of the
    bump pull request, whose second parent is the bump commit. Its own diff is
    the version files, so assessing it would bump again.
    """
    if is_marker_commit(repo, commit):
        return True
    parents = git(repo, "rev-list", "--parents", "-n", "1", commit).stdout.split()[1:]
    return any(is_marker_commit(repo, parent) for parent in parents[1:])


def drain_base(repo: Path, head: str, fallback: str | None) -> str:
    """Where the unassigned range starts: the latest bump reachable from main.

    When the bump reached main through a pull request, the bump commit is the
    merge's second parent, so ``bump..head`` still holds every change that
    landed while that pull request was open; ``plan`` skips the merge itself.
    """
    found = marker(repo, head)
    if found:
        return found
    if fallback and git(repo, "merge-base", "--is-ancestor", fallback, head,
                        check=False).returncode == 0:
        return fallback
    return git(repo, "rev-parse", f"{head}^1").stdout.strip()


def plan(repo: Path, config: Config, base: str, head: str) -> list[Assignment]:
    """Assess every change that landed on main since `base` on its own, then
    take the highest level per surface.

    A `Version-Bump: <surface>=skip` trailer covers only the change that
    carries it. Judging `base..head` as one range let a single skipped change
    suppress every later release until the next marker, so nothing ever
    released again.
    """
    levels: dict[str, str] = {}
    landed = git(repo, "rev-list", "--first-parent", "--reverse",
                 f"{base}..{head}").stdout.split()
    for commit in landed:
        if is_bump_landing(repo, commit):
            continue
        parent = git(repo, "rev-parse", f"{commit}^1").stdout.strip()
        changed = git_diff_names(parent, commit)
        for verdict in assess_surfaces(config, changed, parent, commit, repo):
            name = verdict.surface.name
            levels[name] = max_level(levels.get(name, "none"), verdict.final_level)
    assignments: list[Assignment] = []
    for surface in config.surfaces:
        level = levels.get(surface.name, "none")
        if level == "none":
            continue
        # The version on main's tip is the last one assigned: version files
        # change only through a bump, and the range base predates the bump
        # that covered it.
        current = next((version_at_base(head, vf)
                        for vf in surface.version_files
                        if version_at_base(head, vf)), None)
        if not current:
            current = next((read_version(repo, vf)
                            for vf in surface.version_files
                            if read_version(repo, vf)), None)
        if current:
            assignments.append(Assignment(
                surface.name, level, current, bump_version(current, level)))
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


def sync_trailers(repo: Path, edited: list[str]) -> list[str]:
    """Skip trailers the skill-sync and doc-sync gates need for these files.

    The bump commit edits only version files, which the skill map and doc map
    also claim (Cargo.toml, .claude-plugin/**). The gates would otherwise fail
    the bump pull request, and the steward admits only a CLEAN one. The
    trailers are derived from the gates' own maps and matchers, so they track
    the maps instead of a hard-coded skill name. A repo without the gate
    scripts or maps has nothing to satisfy.
    """
    trailers: list[str] = []
    try:
        import skill_sync_check as skills
        cfg_path, _ = skills.resolve_config(repo, None)
        if cfg_path is not None:
            cfg = skills.load_config(cfg_path, repo)
            skill_map = skills.load_skill_map(cfg.path_map_file)
            changed = skills.filter_generated(edited, cfg.generated_globs)
            for name, patterns in skill_map.skills.items():
                if any(skills._matches_any(path, patterns) for path in changed):
                    trailers.append(f'{cfg.trailer_skill_update}: skip skill={name} '
                                    f'reason="{SYNC_REASON}"')
    except (ImportError, OSError, ValueError, KeyError):
        pass
    try:
        import doc_sync_check as docs
        doc_map_path = repo / "scripts" / "doc_sync_map.json"
        if doc_map_path.exists():
            for entry in docs.load_doc_map(doc_map_path).docs:
                if any(docs._matches_any(path, entry.paths) for path in edited):
                    trailers.append(f'Doc-Update: skip doc={entry.doc} '
                                    f'reason="{SYNC_REASON}"')
    except (ImportError, OSError, ValueError):
        pass
    return trailers


def message(assignments: list[Assignment], base: str, head: str,
            extra_trailers: list[str] | None = None) -> str:
    summary = "; ".join(
        f"{a.surface} {a.current}->{a.assigned} ({a.level})" for a in assignments
    )
    return ("chore: assign versions at land\n\n"
            f"Assigned from merged range {base[:12]}..{head[:12]}.\n"
            f"{summary}\n\n{MARKER}: {head}\n{RELEASE_TRAILER}\n"
            + "".join(f"{line}\n" for line in extra_trailers or []))


class GitHub:
    """The few REST calls the bump pull request needs, through ``gh api``.

    ``VERSION_AT_LAND_GH`` names the binary (tests point it at a fake). The
    workflow authenticates it with RELEASE_BOT_TOKEN, because events caused by
    the default workflow token start no workflows, and the pull request's
    required checks must run.
    """

    def __init__(self, repo: str, binary: str | None = None) -> None:
        self.repo = repo
        self.binary = binary or os.environ.get("VERSION_AT_LAND_GH", "gh")

    def api(self, *args: str) -> object:
        result = subprocess.run([self.binary, "api", *args], check=True,
                                capture_output=True, text=True)
        return json.loads(result.stdout) if result.stdout.strip() else None

    def open_pull(self, branch: str, base: str) -> int | None:
        owner = self.repo.split("/", 1)[0]
        pulls = self.api(f"repos/{self.repo}/pulls?state=open&base={base}"
                         f"&head={owner}:{branch}&per_page=100")
        numbers = sorted(pull["number"] for pull in pulls or []
                         if isinstance(pull, dict)
                         and (pull.get("head") or {}).get("ref") == branch
                         and ((pull.get("head") or {}).get("repo") or {}).get("full_name")
                         == self.repo)
        return numbers[0] if numbers else None

    def create_pull(self, branch: str, base: str, title: str, body: str) -> int:
        created = self.api("-X", "POST", f"repos/{self.repo}/pulls", "-f", f"title={title}",
                           "-f", f"head={branch}", "-f", f"base={base}", "-f", f"body={body}")
        return int(created["number"])  # type: ignore[index]

    def update_pull(self, number: int, title: str, body: str) -> None:
        self.api("-X", "PATCH", f"repos/{self.repo}/pulls/{number}",
                 "-f", f"title={title}", "-f", f"body={body}")

    def label(self, number: int, label: str) -> None:
        self.api("-X", "POST", f"repos/{self.repo}/issues/{number}/labels",
                 "-f", f"labels[]={label}")

    def handoff(self, sha: str, target_url: str | None) -> None:
        args = ["-X", "POST", f"repos/{self.repo}/statuses/{sha}", "-f", "state=success",
                "-f", f"context={HANDOFF_CONTEXT}", "-f", f"description={HANDOFF_DESCRIPTION}"]
        if target_url:
            args += ["-f", f"target_url={target_url}"]
        self.api(*args)


def pull_title(assignments: list[Assignment]) -> str:
    return "chore: assign versions at land (" + ", ".join(
        f"{a.surface} {a.assigned}" for a in assignments) + ")"


def pull_body(assignments: list[Assignment], base: str, head: str) -> str:
    rows = "\n".join(f"- {a.surface}: {a.current} -> {a.assigned} ({a.level})"
                      for a in assignments)
    return ("Post-merge version bump assigned by `version-at-land.yml`.\n\n"
            f"{rows}\n\n"
            f"Covers every change on main from `{base[:12]}` through `{head}`. "
            "This branch is bot-owned and is rebuilt from main on every landing, so "
            "this one pull request always carries the cumulative bump. It lands only "
            "with green required checks and a reviewer's exact-head approval; its "
            "merge is what `auto-release.yml` tags.\n")


def remote_branch(repo: Path, remote: str, branch: str) -> str | None:
    result = git(repo, "ls-remote", "--heads", remote, f"refs/heads/{branch}", check=False)
    fields = result.stdout.split()
    return fields[0] if result.returncode == 0 and fields else None


def still_current(repo: Path, config: Config, remote: str, pr_branch: str,
                  existing: str, head: str, edited: list[str]) -> bool:
    """Whether the pushed bump already is this bump, so the push can be skipped.

    True when the branch's bump commit sits on a main commit that ``head``
    descends from, nothing landed since then needs a version, and every file
    this run edited is byte-identical on the branch. Merging that commit
    releases the same versions, and the run on its merge re-judges the
    intervening changes, so keeping it loses nothing. It also keeps the head a
    reviewer may already have approved instead of resetting the review on
    every documentation-only landing.
    """
    if git(repo, "fetch", "--quiet", remote, f"refs/heads/{pr_branch}",
           check=False).returncode != 0:
        return False
    if git(repo, "rev-parse", "FETCH_HEAD").stdout.strip() != existing:
        return False
    if not is_marker_commit(repo, existing):
        return False
    # A head built before a required trailer existed would fail the sync gates
    # forever if kept, so it is stale.
    body = git(repo, "log", "-1", "--format=%B", existing).stdout.splitlines()
    if any(line not in body for line in sync_trailers(repo, edited)):
        return False
    parent = git(repo, "rev-parse", f"{existing}^1", check=False).stdout.strip()
    if not parent or git(repo, "merge-base", "--is-ancestor", parent, head,
                         check=False).returncode != 0:
        return False
    if parent != head and plan(repo, config, parent, head):
        return False
    for path in edited:
        ours = git(repo, "rev-parse", f"HEAD:{path}", check=False).stdout.strip()
        theirs = git(repo, "rev-parse", f"{existing}:{path}", check=False).stdout.strip()
        if not ours or ours != theirs:
            return False
    return True


def publish_pull(repo: Path, config: Config, remote: str, branch: str, pr_branch: str,
                 github: GitHub, assignments: list[Assignment], edited: list[str],
                 base: str, head: str, target_url: str | None) -> tuple[str, str]:
    """Point the bot branch at the bump commit and open or update its pull request."""
    existing = remote_branch(repo, remote, pr_branch)
    if existing and still_current(repo, config, remote, pr_branch, existing, head, edited):
        sha = existing
    else:
        # The bot owns this one branch; replacing it is how the bump coalesces.
        # The lease refuses to clobber a head this run did not observe.
        result = git(repo, "push", "--porcelain",
                     f"--force-with-lease=refs/heads/{pr_branch}:{existing or ''}",
                     remote, f"HEAD:refs/heads/{pr_branch}", check=False)
        if result.returncode != 0:
            print(result.stderr or result.stdout, flush=True)
            return "exhausted", ""
        sha = git(repo, "rev-parse", "HEAD").stdout.strip()
    title, body = pull_title(assignments), pull_body(assignments, base, head)
    number = github.open_pull(pr_branch, branch)
    if number is None:
        number = github.create_pull(pr_branch, branch, title, body)
        status = "opened"
    else:
        github.update_pull(number, title, body)
        status = "updated"
    # Hand the exact head to the merge steward. A later push to this branch
    # by anyone else carries no handoff status, so it is not admitted.
    github.label(number, MANAGED_LABEL)
    github.handoff(sha, target_url)
    return status, sha


def run(repo: Path, config: Config, remote: str, branch: str,
        fallback: str | None, retries: int, push: bool,
        pr_branch: str | None = None, github: GitHub | None = None,
        target_url: str | None = None) -> tuple[str, list[Assignment]]:
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
        git(repo, "commit", "--no-verify", "-m",
            message(assignments, base, head, sync_trailers(repo, edited)))
        if pr_branch is not None and github is not None:
            status, _ = publish_pull(repo, config, remote, branch, pr_branch, github,
                                     assignments, edited, base, head, target_url)
            if status != "exhausted":
                return status, assignments
            git(repo, "reset", "--hard", head)
            continue
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
    parser.add_argument("--pr-branch",
                        help="with --push, land the bump through one pull request from "
                             "this bot-owned branch instead of pushing to --branch")
    parser.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY"),
                        help="owner/name for the pull request (default $GITHUB_REPOSITORY)")
    parser.add_argument("--target-url", help="link recorded on the handoff status")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    if args.pr_branch and not args.push:
        parser.error("--pr-branch requires --push")
    if args.pr_branch and not args.repo:
        parser.error("--pr-branch requires --repo or $GITHUB_REPOSITORY")
    if args.pr_branch and args.pr_branch == args.branch:
        parser.error("--pr-branch must differ from --branch")
    root = Path(git(Path.cwd(), "rev-parse", "--show-toplevel").stdout.strip())
    config = load_config((root / args.config).resolve())
    if not args.push and not args.base:
        parser.error("--base is required unless --push is given")
    github = GitHub(args.repo) if args.pr_branch else None
    status, assignments = run(root, config, args.remote, args.branch, args.base,
                              args.max_retries, args.push, args.pr_branch, github,
                              args.target_url)
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

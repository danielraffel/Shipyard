#!/usr/bin/env python3
"""Refuse closing a PR whose exact head is not contained by its live base."""

from __future__ import annotations

import json
import os
import pathlib
import re
import subprocess
import sys
from collections.abc import Callable
from typing import Any, NamedTuple
from urllib.parse import quote


class GuardError(RuntimeError):
    """The guard could not obtain trustworthy closure evidence."""


class NotFound(GuardError):
    """GitHub reported that a pinned resource does not exist."""


class CloseRequest(NamedTuple):
    repo: str
    pr: int
    allow_non_pr: bool = False


ApiJson = Callable[[str], dict[str, Any]]


def option_value(args: list[str], names: set[str]) -> str | None:
    for index, arg in enumerate(args):
        if arg in names and index + 1 < len(args):
            return args[index + 1]
        for name in names:
            prefix = f"{name}="
            if arg.startswith(prefix):
                return arg.removeprefix(prefix)
    return None


def field_values(args: list[str]) -> list[str]:
    values: list[str] = []
    names = {"-f", "-F", "--field", "--raw-field"}
    for index, arg in enumerate(args):
        if arg in names and index + 1 < len(args):
            values.append(args[index + 1])
        elif any(arg.startswith(f"{name}=") for name in names):
            values.append(arg.split("=", 1)[1])
    return values


def graphql_document(args: list[str]) -> str:
    values = field_values(args)
    for value in values:
        query = value.removeprefix("query=")
        if value.startswith("query=@"):
            try:
                return pathlib.Path(query[1:]).read_text(encoding="utf-8")
            except OSError:
                return ""
        if value.startswith("query="):
            return query
    path = option_value(args, {"--input"})
    if path and path != "-":
        try:
            return pathlib.Path(path).read_text(encoding="utf-8")
        except OSError:
            return ""
    return ""


def parse_pr_number(value: str) -> int | None:
    if value.isdigit():
        return int(value)
    match = re.fullmatch(r"https://github\.com/([^/]+/[^/]+)/pull/(\d+)/?", value)
    if match:
        return int(match.group(2))
    return None


def repo_from_url(value: str) -> str | None:
    match = re.fullmatch(r"https://github\.com/([^/]+/[^/]+)/pull/\d+/?", value)
    return match.group(1) if match else None


def pr_close_target(args: list[str]) -> str:
    value_options = {"-c", "--comment", "-R", "--repo"}
    skip_next = False
    for arg in args[2:]:
        if skip_next:
            skip_next = False
            continue
        if arg in value_options:
            skip_next = True
            continue
        if any(arg.startswith(f"{name}=") for name in value_options):
            continue
        if arg.startswith("-"):
            continue
        return arg
    raise GuardError("PR close target is missing")


def normalize_repo(value: str) -> str:
    parts = value.removeprefix("https://github.com/").removesuffix(".git").strip("/").split("/")
    if len(parts) == 3 and "." in parts[0]:
        parts = parts[1:]
    if len(parts) != 2 or not all(parts):
        raise GuardError(f"invalid repository identity: {value}")
    return "/".join(parts)


def input_closes_pr(args: list[str]) -> bool:
    path = option_value(args, {"--input"})
    if path is None:
        return False
    if path == "-":
        raise GuardError("cannot inspect a PR PATCH body read from stdin")
    try:
        value = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise GuardError(f"cannot inspect PR PATCH body: {error}") from error
    return isinstance(value, dict) and str(value.get("state", "")).lower() == "closed"


def close_request(args: list[str]) -> CloseRequest | None:
    if len(args) >= 2 and args[:2] in (["pr", "close"], ["issue", "close"]):
        target = pr_close_target(args)
        pr = parse_pr_number(target)
        if pr is None:
            raise GuardError("PR close target must be a number or GitHub pull-request URL")
        repo = option_value(args, {"-R", "--repo"}) or repo_from_url(target)
        if repo is None:
            repo = live_repo()
        return CloseRequest(
            repo=normalize_repo(repo),
            pr=pr,
            allow_non_pr=args[0] == "issue",
        )

    if len(args) >= 2 and args[0] == "api":
        if args[1] == "graphql":
            compact = "".join(graphql_document(args).split()).lower()
            if "closepullrequest(" in compact:
                raise GuardError(
                    "raw closePullRequest mutation cannot bind integrated proof to a PR number"
                )
            return None
        method = (option_value(args, {"-X", "--method"}) or "GET").upper()
        endpoint = next((arg for arg in args[1:] if arg.startswith("repos/")), "")
        match = re.fullmatch(r"repos/([^/]+/[^/]+)/(pulls|issues)/(\d+)", endpoint)
        closes = any(value.lower() == "state=closed" for value in field_values(args))
        closes = closes or input_closes_pr(args)
        if method == "PATCH" and match and closes:
            return CloseRequest(
                repo=normalize_repo(match.group(1)),
                pr=int(match.group(3)),
                allow_non_pr=match.group(2) == "issues",
            )
    return None


def real_gh() -> str:
    configured = os.environ.get("GHAPP_REAL_GH")
    if configured:
        return configured
    homebrew = pathlib.Path("/opt/homebrew/bin/gh")
    if homebrew.is_file():
        return str(homebrew)
    raise GuardError("GHAPP_REAL_GH is unset and /opt/homebrew/bin/gh is unavailable")


def command_json(arguments: list[str]) -> dict[str, Any]:
    try:
        result = subprocess.run(
            [real_gh(), *arguments],
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise GuardError(str(error)) from error
    if result.returncode != 0:
        detail = result.stderr.strip() or f"exit {result.returncode}"
        if "HTTP 404" in detail:
            raise NotFound(detail)
        raise GuardError(detail)
    try:
        value = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise GuardError("GitHub returned malformed JSON") from error
    if not isinstance(value, dict):
        raise GuardError("GitHub returned a non-object response")
    return value


def api_json(endpoint: str) -> dict[str, Any]:
    return command_json(["api", endpoint])


def live_repo() -> str:
    value = command_json(["repo", "view", "--json", "nameWithOwner"])
    repo = value.get("nameWithOwner")
    if not isinstance(repo, str) or repo.count("/") != 1:
        raise GuardError("repository identity is unavailable")
    return repo


def required_string(value: Any, path: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise GuardError(f"response missing {path}")
    return value


def required_count(value: Any, path: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise GuardError(f"response missing valid {path}")
    return value


def head_is_contained(comparison: dict[str, Any]) -> bool:
    """Interpret GitHub's `current-base...PR-head` comparison."""

    status = required_string(comparison.get("status"), "status").lower()
    ahead_by = required_count(comparison.get("ahead_by"), "ahead_by")
    behind_by = required_count(comparison.get("behind_by"), "behind_by")
    valid_shape = {
        "ahead": ahead_by > 0 and behind_by == 0,
        "behind": ahead_by == 0 and behind_by > 0,
        "identical": ahead_by == 0 and behind_by == 0,
        "diverged": ahead_by > 0 and behind_by > 0,
    }
    if status not in valid_shape or not valid_shape[status]:
        raise GuardError(
            f"contradictory comparison status={status} "
            f"ahead_by={ahead_by} behind_by={behind_by}"
        )
    return ahead_by == 0 and status in {"behind", "identical"}


def path_blob_sha(repo: str, path: str, ref: str, query: ApiJson) -> str | None:
    endpoint = f"repos/{repo}/contents/{quote(path, safe='/')}?ref={ref}"
    try:
        content = query(endpoint)
    except NotFound:
        return None
    return required_string(content.get("sha"), f"contents sha for {path}")


def changed_content_is_contained(
    repo: str,
    base_sha: str,
    comparison: dict[str, Any],
    query: ApiJson,
) -> bool:
    """Prove every PR-side changed path already has its exact base-side content."""

    files = comparison.get("files")
    if not isinstance(files, list):
        return False
    # GitHub caps compare-file output at 300 entries. Exactly 300 is therefore
    # ambiguous and cannot prove the complete patch is contained.
    if len(files) >= 300:
        return False
    for item in files:
        if not isinstance(item, dict):
            raise GuardError("comparison files contains a non-object entry")
        status = required_string(item.get("status"), "files.status").lower()
        filename = required_string(item.get("filename"), "files.filename")
        live_sha = path_blob_sha(repo, filename, base_sha, query)
        if status == "removed":
            if live_sha is not None:
                return False
            continue
        if status == "renamed":
            previous = required_string(item.get("previous_filename"), "files.previous_filename")
            if previous != filename and path_blob_sha(repo, previous, base_sha, query) is not None:
                return False
        elif status not in {"added", "modified", "changed", "copied", "unchanged"}:
            raise GuardError(f"unsupported comparison file status: {status}")
        expected_sha = required_string(item.get("sha"), f"files.sha for {filename}")
        if live_sha != expected_sha:
            return False
    return True


def containment_evidence(request: CloseRequest, query: ApiJson) -> tuple[bool, str]:
    try:
        pull = query(f"repos/{request.repo}/pulls/{request.pr}")
    except NotFound:
        if request.allow_non_pr:
            issue = query(f"repos/{request.repo}/issues/{request.pr}")
            if "pull_request" not in issue:
                return True, "target is positively identified as an issue, not a pull request"
            raise GuardError("issue alias identifies a pull request that could not be inspected")
        raise
    head_object = pull.get("head")
    base_object = pull.get("base")
    if not isinstance(head_object, dict) or not isinstance(base_object, dict):
        raise GuardError("pull-request response missing head/base")
    head_sha = required_string(head_object.get("sha"), "head.sha")
    base_ref = required_string(base_object.get("ref"), "base.ref")
    base_commit = query(f"repos/{request.repo}/commits/{quote(base_ref, safe='')}")
    base_sha = required_string(base_commit.get("sha"), "base commit sha")

    # Direction is load-bearing: GitHub describes the second operand relative
    # to the first. Always ask current-base...PR-head. A status of `ahead`
    # therefore means the PR still owns unique commits and must stay open.
    comparison = query(f"repos/{request.repo}/compare/{base_sha}...{head_sha}")
    contained = head_is_contained(comparison)
    content_contained = contained or changed_content_is_contained(
        request.repo, base_sha, comparison, query
    )
    status = required_string(comparison.get("status"), "status").lower()
    ahead_by = required_count(comparison.get("ahead_by"), "ahead_by")
    behind_by = required_count(comparison.get("behind_by"), "behind_by")
    detail = (
        f"base={base_sha} head={head_sha} status={status} "
        f"ahead_by={ahead_by} behind_by={behind_by}"
    )
    return content_contained, detail


def main(args: list[str], *, api_json: ApiJson = api_json) -> int:
    try:
        request = close_request(args)
    except GuardError as error:
        print(f"pr-close-guard: refusing ambiguous PR close: {error}", file=sys.stderr)
        return 1
    if request is None:
        return 0
    if os.environ.get("GHAPP_ALLOW_UNINTEGRATED_PR_CLOSE") == "1":
        print(
            "pr-close-guard: WARNING: explicit override permits closing a PR "
            "without integrated-head proof",
            file=sys.stderr,
        )
        return 0
    try:
        contained, detail = containment_evidence(request, api_json)
    except NotFound as error:
        print(
            f"pr-close-guard: refusing PR #{request.pr}; could not prove its exact "
            f"head is contained by the live base: {error}",
            file=sys.stderr,
        )
        return 1
    except GuardError as error:
        print(
            f"pr-close-guard: refusing PR #{request.pr}; could not prove its exact "
            f"head is contained by the live base: {error}",
            file=sys.stderr,
        )
        return 1
    if contained:
        return 0
    comparison = detail.split(" ")
    ahead = next(part.split("=", 1)[1] for part in comparison if part.startswith("ahead_by="))
    status = next(part.split("=", 1)[1] for part in comparison if part.startswith("status="))
    print(
        f"pr-close-guard: refusing PR #{request.pr}; current-base...PR-head is "
        f"{status} with {ahead} unique commit(s) on the PR head ({detail}). "
        "Use GHAPP_ALLOW_UNINTEGRATED_PR_CLOSE=1 only for an explicit "
        "abandonment or sequence-lock authority action.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

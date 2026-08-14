#!/usr/bin/env python3
"""Refuse closing a PR whose exact head is not contained by its live base."""

from __future__ import annotations

import json
import os
import pathlib
import posixpath
import re
import subprocess
import sys
from collections.abc import Callable
from typing import Any, NamedTuple
from urllib.parse import parse_qs, quote, unquote, urlsplit


class GuardError(RuntimeError):
    """The guard could not obtain trustworthy closure evidence."""


class NotFound(GuardError):
    """GitHub reported that a pinned resource does not exist."""


class CloseRequest(NamedTuple):
    repo: str
    pr: int
    allow_non_pr: bool = False
    hostname: str | None = None


class ApiTarget(NamedTuple):
    path: str
    query: dict[str, list[str]]
    hostname: str | None = None


ApiJson = Callable[[str], dict[str, Any]]


def option_values(args: list[str], names: set[str]) -> list[str]:
    values: list[str] = []
    for index, arg in enumerate(args):
        for name in names:
            if arg == name and index + 1 < len(args):
                values.append(args[index + 1])
                break
            if name.startswith("--") and arg.startswith(f"{name}="):
                values.append(arg.removeprefix(f"{name}="))
                break
            if len(name) == 2 and name.startswith("-") and arg.startswith(name) and len(arg) > 2:
                values.append(arg[len(name) :].removeprefix("="))
                break
    return values


def option_value(args: list[str], names: set[str]) -> str | None:
    values = option_values(args, names)
    return values[-1] if values else None


def field_values(args: list[str]) -> list[str]:
    return option_values(args, {"-f", "-F", "--field", "--raw-field"})


def graphql_document(args: list[str]) -> str:
    documents: list[str] = []
    for value in field_values(args):
        if not value.startswith("query="):
            continue
        query = value.removeprefix("query=")
        if query == "@-":
            raise GuardError("cannot inspect a GraphQL body read from stdin")
        if query.startswith("@"):
            try:
                documents.append(pathlib.Path(query[1:]).read_text(encoding="utf-8"))
            except OSError as error:
                raise GuardError(f"cannot inspect GraphQL query file: {error}") from error
        else:
            documents.append(query)
    path = option_value(args, {"--input"})
    if path == "-":
        raise GuardError("cannot inspect a GraphQL body read from stdin")
    if path:
        try:
            body = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
        except OSError as error:
            raise GuardError(f"cannot inspect GraphQL input file: {error}") from error
        except json.JSONDecodeError as error:
            raise GuardError(f"cannot inspect malformed GraphQL input: {error}") from error
        if not isinstance(body, dict) or not isinstance(body.get("query"), str):
            raise GuardError("GraphQL input must be a JSON object with a string query")
        documents.append(body["query"])
    return "\n".join(documents)


def api_target(args: list[str]) -> ApiTarget:
    hostname = option_value(args, {"--hostname"})
    if hostname is not None:
        hostname = hostname.lower()
        if hostname in {"github.com", "api.github.com"}:
            hostname = None
    value_options = {
        "--cache",
        "-F",
        "--field",
        "-H",
        "--header",
        "--hostname",
        "--input",
        "-q",
        "--jq",
        "-X",
        "--method",
        "-p",
        "--preview",
        "-f",
        "--raw-field",
        "-t",
        "--template",
    }
    skip_next = False
    for arg in args[1:]:
        if skip_next:
            skip_next = False
            continue
        if arg in value_options:
            skip_next = True
            continue
        if any(
            (name.startswith("--") and arg.startswith(f"{name}="))
            or (len(name) == 2 and arg.startswith(name) and len(arg) > 2)
            for name in value_options
        ):
            continue
        if arg.startswith("-"):
            continue
        parts = urlsplit(arg)
        path = parts.path
        if parts.scheme or parts.netloc:
            try:
                port = parts.port
            except ValueError as error:
                raise GuardError("cannot inspect absolute API endpoint") from error
            if (
                parts.scheme.lower() != "https"
                or parts.hostname is None
                or parts.hostname.lower() != "api.github.com"
                or port not in (None, 443)
                or parts.username is not None
                or parts.password is not None
            ):
                raise GuardError("cannot inspect absolute API endpoint")
        if re.search(r"%(?![0-9A-Fa-f]{2})", path):
            raise GuardError("cannot inspect malformed encoded API endpoint")
        path = posixpath.normpath(unquote(path)).lstrip("/")
        path = resolve_api_placeholders(path)
        return ApiTarget(path, parse_qs(parts.query, keep_blank_values=True), hostname)
    return ApiTarget("", {}, hostname)


def resolve_api_placeholders(path: str) -> str:
    if "{" not in path and "}" not in path:
        return path
    repo_parts = os.environ.get("GH_REPO", "").split("/")
    if len(repo_parts) >= 2:
        owner, repo = repo_parts[-2], repo_parts[-1]
    else:
        owner, repo = live_repo().split("/", 1)
    branch = ""
    if "{branch}" in path:
        try:
            branch = subprocess.run(
                ["git", "branch", "--show-current"],
                check=True,
                capture_output=True,
                text=True,
                timeout=10,
            ).stdout.strip()
        except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
            raise GuardError(f"cannot resolve API endpoint branch placeholder: {error}") from error
    for placeholder, value in {
        "{owner}": owner,
        "{repo}": repo,
        "{branch}": branch,
    }.items():
        if placeholder in path:
            if not value:
                raise GuardError(f"cannot resolve API endpoint placeholder {placeholder}")
            path = path.replace(placeholder, value)
    if "{" in path or "}" in path:
        raise GuardError("cannot resolve unknown API endpoint placeholder")
    return path


def compact_graphql(document: str) -> str:
    """Remove insignificant GraphQL text without mistaking comments for syntax."""
    without_comments: list[str] = []
    in_string = False
    escaped = False
    index = 0
    while index < len(document):
        char = document[index]
        if in_string:
            without_comments.append(char)
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                in_string = False
            index += 1
            continue
        if char == '"':
            in_string = True
            without_comments.append(char)
            index += 1
            continue
        if char == "#":
            while index < len(document) and document[index] not in "\r\n":
                index += 1
            continue
        if char == ",":
            index += 1
            continue
        without_comments.append(char)
        index += 1
    return "".join("".join(without_comments).split()).lower()


def typed_field_closes_pr(args: list[str]) -> bool:
    for value in option_values(args, {"-F", "--field"}):
        if not value.startswith("state="):
            continue
        state = value.removeprefix("state=")
        if state == "@-":
            raise GuardError("cannot inspect a PR state field read from stdin")
        if state.startswith("@"):
            try:
                state = pathlib.Path(state[1:]).read_text(encoding="utf-8").strip()
            except OSError as error:
                raise GuardError(f"cannot inspect PR state field file: {error}") from error
        if state.lower() == "closed":
            return True
    return False


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


def close_subcommand_index(args: list[str]) -> int | None:
    value_options = {"-R", "--repo"}
    skip_next = False
    for index, arg in enumerate(args[1:], start=1):
        if skip_next:
            skip_next = False
            continue
        if arg in value_options:
            skip_next = True
            continue
        if any(arg.startswith(f"{name}=") for name in value_options):
            continue
        if arg == "close":
            return index
        if arg.startswith("-"):
            continue
        return None
    return None


def pr_close_target(args: list[str], close_index: int) -> str:
    value_options = {"-c", "--comment", "-R", "--repo"}
    skip_next = False
    for arg in args[close_index + 1 :]:
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


def repo_identity(value: str) -> tuple[str, str | None]:
    parts = value.removeprefix("https://github.com/").removesuffix(".git").strip("/").split("/")
    hostname = None
    if len(parts) == 3 and "." in parts[0]:
        hostname = parts[0].lower()
        parts = parts[1:]
    if len(parts) != 2 or not all(parts):
        raise GuardError(f"invalid repository identity: {value}")
    if hostname in {"github.com", "api.github.com"}:
        hostname = None
    return "/".join(parts), hostname


def normalize_repo(value: str) -> str:
    return repo_identity(value)[0]


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
    close_index = close_subcommand_index(args) if args and args[0] in {"pr", "issue"} else None
    if close_index is not None:
        target = pr_close_target(args, close_index)
        pr = parse_pr_number(target)
        if pr is None:
            raise GuardError("PR close target must be a number or GitHub pull-request URL")
        repo = option_value(args, {"-R", "--repo"}) or repo_from_url(target)
        if repo is None:
            repo = live_repo()
        repo, hostname = repo_identity(repo)
        return CloseRequest(
            repo=repo,
            pr=pr,
            allow_non_pr=args[0] == "issue",
            hostname=hostname,
        )

    if len(args) >= 2 and args[0] == "api":
        target = api_target(args)
        endpoint = target.path
        if endpoint == "graphql":
            document = "\n".join([graphql_document(args), *target.query.get("query", [])])
            compact = compact_graphql(document)
            if "closepullrequest(" in compact:
                raise GuardError(
                    "raw closePullRequest mutation cannot bind integrated proof to a PR number"
                )
            return None
        method = (option_value(args, {"-X", "--method"}) or "GET").upper()
        match = re.fullmatch(r"repos/([^/]+/[^/]+)/(pulls|issues)/(\d+)", endpoint)
        if method == "PATCH" and match:
            closes = any(
                value.lower() == "state=closed" for value in field_values(args)
            ) or any(value.lower() == "closed" for value in target.query.get("state", []))
            closes = closes or typed_field_closes_pr(args) or input_closes_pr(args)
        else:
            closes = False
        if closes and match:
            return CloseRequest(
                repo=normalize_repo(match.group(1)),
                pr=int(match.group(3)),
                allow_non_pr=match.group(2) == "issues",
                hostname=target.hostname,
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


def evidence_query(request: CloseRequest, fallback: ApiJson) -> ApiJson:
    if request.hostname is None:
        return fallback
    return lambda endpoint: command_json(
        ["api", "--hostname", request.hostname, endpoint]
    )


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


def tree_entry_for_path(
    repo: str, commit_sha: str, path: str, query: ApiJson
) -> tuple[str, str, str] | None:
    tree_sha = commit_sha
    components = path.split("/")
    for index, component in enumerate(components):
        value = query(f"repos/{repo}/git/trees/{tree_sha}")
        if value.get("truncated") is not False:
            raise GuardError("Git tree evidence is truncated or ambiguous")
        items = value.get("tree")
        if not isinstance(items, list):
            raise GuardError("Git tree response is missing entries")
        entry = next(
            (
                item
                for item in items
                if isinstance(item, dict) and item.get("path") == component
            ),
            None,
        )
        if entry is None:
            return None
        mode = required_string(entry.get("mode"), f"tree.mode for {path}")
        kind = required_string(entry.get("type"), f"tree.type for {path}")
        sha = required_string(entry.get("sha"), f"tree.sha for {path}")
        if index == len(components) - 1:
            return mode, kind, sha
        if kind != "tree":
            return None
        tree_sha = sha
    return None


def changed_content_is_contained(
    repo: str,
    base_sha: str,
    head_sha: str,
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
        base_entry = tree_entry_for_path(repo, base_sha, filename, query)
        head_entry = tree_entry_for_path(repo, head_sha, filename, query)
        if status == "removed":
            if base_entry is not None or head_entry is not None:
                return False
            continue
        if status == "renamed":
            previous = required_string(item.get("previous_filename"), "files.previous_filename")
            if previous != filename and tree_entry_for_path(
                repo, base_sha, previous, query
            ) is not None:
                return False
        elif status not in {"added", "modified", "changed", "copied", "unchanged"}:
            raise GuardError(f"unsupported comparison file status: {status}")
        expected_sha = required_string(item.get("sha"), f"files.sha for {filename}")
        if head_entry is None or head_entry[2] != expected_sha or base_entry != head_entry:
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
        request.repo, base_sha, head_sha, comparison, query
    )
    status = required_string(comparison.get("status"), "status").lower()
    ahead_by = required_count(comparison.get("ahead_by"), "ahead_by")
    behind_by = required_count(comparison.get("behind_by"), "behind_by")
    detail = (
        f"base={base_sha} head={head_sha} status={status} "
        f"ahead_by={ahead_by} behind_by={behind_by}"
    )

    confirmation = query(f"repos/{request.repo}/pulls/{request.pr}")
    confirmed_head = confirmation.get("head")
    confirmed_base = confirmation.get("base")
    if not isinstance(confirmed_head, dict) or not isinstance(confirmed_base, dict):
        raise GuardError("confirmation response missing head/base")
    confirmed_head_sha = required_string(confirmed_head.get("sha"), "confirmed head.sha")
    confirmed_base_ref = required_string(confirmed_base.get("ref"), "confirmed base.ref")
    confirmed_base_commit = query(
        f"repos/{request.repo}/commits/{quote(confirmed_base_ref, safe='')}"
    )
    confirmed_base_sha = required_string(
        confirmed_base_commit.get("sha"), "confirmed base commit sha"
    )
    if (confirmed_head_sha, confirmed_base_ref, confirmed_base_sha) != (
        head_sha,
        base_ref,
        base_sha,
    ):
        raise GuardError("PR head or base moved during closure proof; retry")
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
    query = evidence_query(request, api_json)
    try:
        contained, detail = containment_evidence(request, query)
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

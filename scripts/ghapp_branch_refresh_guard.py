#!/usr/bin/env python3
"""Refuse a routine branch refresh that a merge queue makes pointless.

Intercepted (through the App-authenticated ``ghapp`` wrapper):

* ``pr update-branch [<pr>] [--rebase]``;
* ``api`` REST calls to ``repos/<owner>/<repo>/pulls/<n>/update-branch``;
* ``api graphql`` documents containing ``updatePullRequestBranch``.

Every one of those pushes a new head to the pull request. On a repository whose
workflows key their concurrency on the PR ref with ``cancel-in-progress``, the
push cancels the required gate already running on the old head and starts it
again. When the base branch lands through a GitHub merge queue that is pure
waste: the queue builds and validates the merge result itself, so bringing the
branch up to date first buys nothing.

The decision follows the repository's ``[merge] refresh_branch`` policy, read
from ``.shipyard/config.toml`` on the pull request's base branch (so a PR
branch cannot relax it for itself):

* ``"always"`` (the default, and what an absent key or file means): allow every
  refresh. This is Shipyard's historical behaviour exactly.
* ``"only-if-conflicting"``: refuse a refresh only when every one of these is
  proven: the base branch has a merge queue, the PR is not conflicting, and no
  required check on the PR head is failing. A conflicting PR, or one whose
  required check is red (the queue would reject it; a fresh merge ref can clear
  a failure at a step the current workflow no longer runs), is still refreshed.
  Anything the guard cannot read is allowed, never refused: skipping a refresh
  that correctness needs is worse than a wasted gate run.

An unrecognized policy value is reported and treated as ``"always"``.
"""

from __future__ import annotations

import importlib.machinery
import importlib.util
import json
import os
import pathlib
import re
import subprocess
import sys
import time
from types import ModuleType
from typing import Any

POLICY_KEY = "refresh_branch"
POLICY_ALWAYS = "always"
POLICY_ONLY_IF_CONFLICTING = "only-if-conflicting"
POLICIES = (POLICY_ALWAYS, POLICY_ONLY_IF_CONFLICTING)
CONFIG_PATH = ".shipyard/config.toml"

# Required-check outcomes after which the merge queue would refuse the PR, so a
# refresh (which re-runs every check under the current workflow) is warranted.
FAILING_CONCLUSIONS = {
    "ACTION_REQUIRED",
    "CANCELLED",
    "FAILURE",
    "STALE",
    "STARTUP_FAILURE",
    "TIMED_OUT",
}
FAILING_STATUS_STATES = {"ERROR", "FAILURE"}
UNKNOWN_MERGEABLE_RETRIES = 2
UNKNOWN_MERGEABLE_DELAY_SECONDS = 2.0

_PR_FIELDS = (
    "number state baseRefName headRefOid mergeable mergeStateStatus isInMergeQueue "
    "commits(last:1){nodes{commit{statusCheckRollup{contexts(first:100){"
    "pageInfo{hasNextPage} nodes{__typename "
    "... on CheckRun{name conclusion status isRequired(pullRequestNumber:$number)} "
    "... on StatusContext{context state isRequired(pullRequestNumber:$number)}}}}}}}"
)
PR_QUERY = (
    "query($owner:String!,$name:String!,$number:Int!,$base:String!)"
    "{repository(owner:$owner,name:$name){mergeQueue(branch:$base){id} "
    "pullRequest(number:$number){" + _PR_FIELDS + "}}}"
)
PR_ID_QUERY = (
    "query($id:ID!){node(id:$id){... on PullRequest{number repository{nameWithOwner}}}}"
)


class GuardError(RuntimeError):
    """The guard could not read something it needs."""


def _load_request_parser() -> ModuleType | None:
    """Reuse the removal guard's request parser rather than a second copy."""
    here = pathlib.Path(__file__).resolve().parent
    for name in ("ghapp_queue_removal_guard.py", "queue-removal-guard"):
        candidate = here / name
        if not candidate.is_file():
            continue
        loader = importlib.machinery.SourceFileLoader("_ghapp_refresh_request_parser", str(candidate))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        if spec is None:
            continue
        module = importlib.util.module_from_spec(spec)
        try:
            loader.exec_module(module)
        except Exception:  # noqa: BLE001 - any failure means "parser unavailable"
            continue
        if all(
            hasattr(module, attribute)
            for attribute in ("api_target", "graphql_document", "option_values", "GuardError")
        ):
            return module
    return None


PARSER = _load_request_parser()


# ---------------------------------------------------------------------------
# Policy.
# ---------------------------------------------------------------------------


def _strip_toml_comment(line: str) -> str:
    quote = None
    for index, char in enumerate(line):
        if quote:
            if char == quote:
                quote = None
        elif char in "\"'":
            quote = char
        elif char == "#":
            return line[:index]
    return line


def policy_from_toml(text: str) -> str | None:
    """Return ``[merge] refresh_branch`` from config text, or ``None`` when unset.

    ``tomllib`` is used when the interpreter has it (3.11+); older interpreters
    fall back to a scanner that understands exactly this one string key.
    """
    try:
        import tomllib  # type: ignore[import-not-found]
    except ImportError:
        tomllib = None
    if tomllib is not None:
        try:
            data = tomllib.loads(text)
        except tomllib.TOMLDecodeError as error:
            raise GuardError(f"{CONFIG_PATH} is not valid TOML: {error}") from error
        merge = data.get("merge")
        value = merge.get(POLICY_KEY) if isinstance(merge, dict) else None
        if value is None:
            return None
        if not isinstance(value, str):
            raise GuardError(f"merge.{POLICY_KEY} must be a string")
        return value
    section = None
    for raw in text.splitlines():
        line = _strip_toml_comment(raw).strip()
        if not line:
            continue
        header = re.fullmatch(r"\[\s*([A-Za-z0-9_.\-\"]+)\s*\]", line)
        if header:
            section = header.group(1).replace('"', "")
            continue
        if section != "merge":
            continue
        match = re.fullmatch(rf"{POLICY_KEY}\s*=\s*(\"([^\"]*)\"|'([^']*)')", line)
        if match:
            return match.group(2) if match.group(2) is not None else match.group(3)
        if re.match(rf"{POLICY_KEY}\s*=", line):
            raise GuardError(f"merge.{POLICY_KEY} must be a string")
    return None


def normalize_policy(value: str | None) -> tuple[str, str | None]:
    """Return ``(policy, warning)``; an unknown value degrades to ``always``."""
    if value is None:
        return POLICY_ALWAYS, None
    if value in POLICIES:
        return value, None
    return POLICY_ALWAYS, (
        f"ignoring unrecognized merge.{POLICY_KEY} = {value!r} "
        f"(expected one of {', '.join(POLICIES)}); treating it as {POLICY_ALWAYS!r}"
    )


# ---------------------------------------------------------------------------
# Decision: pure, so it is tested without any network.
# ---------------------------------------------------------------------------


def _pull_request(response: Any) -> dict[str, Any] | None:
    if not isinstance(response, dict):
        return None
    data = response.get("data")
    if not isinstance(data, dict):
        return None
    repository = data.get("repository")
    if isinstance(repository, dict) and isinstance(repository.get("pullRequest"), dict):
        return repository["pullRequest"]
    return None


def refresh_facts(response: Any) -> dict[str, Any]:
    """Extract the facts ``decide`` needs. Any unreadable fact is ``None``."""
    pr = _pull_request(response)
    if pr is None or (isinstance(response, dict) and response.get("errors")):
        return {"readable": False}
    repository = response["data"]["repository"]
    has_queue = None
    if "mergeQueue" in repository:
        has_queue = repository["mergeQueue"] is not None
    mergeable = pr.get("mergeable") if isinstance(pr.get("mergeable"), str) else None
    merge_state = (
        pr.get("mergeStateStatus") if isinstance(pr.get("mergeStateStatus"), str) else None
    )
    if mergeable == "CONFLICTING" or merge_state == "DIRTY":
        conflicting: bool | None = True
    elif mergeable == "MERGEABLE":
        conflicting = False
    else:
        conflicting = None

    failing: list[str] = []
    checks_readable = True
    try:
        commit = pr["commits"]["nodes"][0]["commit"]
        rollup = commit.get("statusCheckRollup")
        if rollup is None:
            contexts: list[Any] = []
        else:
            connection = rollup["contexts"]
            if connection["pageInfo"]["hasNextPage"]:
                checks_readable = False
            contexts = connection["nodes"]
    except (KeyError, IndexError, TypeError):
        checks_readable = False
        contexts = []
    for node in contexts:
        if not isinstance(node, dict) or node.get("isRequired") is not True:
            continue
        if node.get("__typename") == "CheckRun":
            if str(node.get("conclusion") or "").upper() in FAILING_CONCLUSIONS:
                failing.append(str(node.get("name")))
        elif node.get("__typename") == "StatusContext":
            if str(node.get("state") or "").upper() in FAILING_STATUS_STATES:
                failing.append(str(node.get("context")))
    return {
        "readable": True,
        "pr": pr.get("number"),
        "state": pr.get("state"),
        "base": pr.get("baseRefName"),
        "has_merge_queue": has_queue,
        "in_merge_queue": pr.get("isInMergeQueue") is True,
        "conflicting": conflicting,
        "mergeable": mergeable,
        "merge_state": merge_state,
        "failing_required": sorted(failing) if checks_readable else None,
    }


def decide(policy: str, facts: dict[str, Any]) -> tuple[bool, str]:
    """Return ``(allowed, reason)`` for one refresh request under ``policy``."""
    if policy != POLICY_ONLY_IF_CONFLICTING:
        return True, f"merge.{POLICY_KEY} is {policy!r}"
    if not facts.get("readable"):
        return True, "the pull request's live state could not be read; not refusing blind"
    label = f"PR #{facts.get('pr', '?')}"
    if str(facts.get("state") or "").upper() != "OPEN":
        return True, f"{label} is not open; nothing for this guard to protect"
    if facts.get("has_merge_queue") is None:
        return True, f"cannot tell whether {label}'s base has a merge queue"
    if not facts["has_merge_queue"]:
        return True, (
            f"{label}'s base `{facts.get('base')}` has no merge queue, so an up-to-date head "
            "may be what lets it merge"
        )
    if facts.get("conflicting") is None:
        return True, f"GitHub has not computed whether {label} conflicts (mergeable=UNKNOWN)"
    if facts["conflicting"]:
        return True, f"{label} conflicts with its base; resolving that needs a new head"
    failing = facts.get("failing_required")
    if failing is None:
        return True, f"{label}'s required checks could not all be read"
    if failing:
        return True, (
            f"{label} has failing required checks ({', '.join(failing)}), which the merge queue "
            "would reject"
        )
    where = " and is already in the merge queue" if facts.get("in_merge_queue") else ""
    return False, (
        f"{label} is mergeable{where}, and `{facts.get('base')}` lands through a merge queue "
        "that validates the merge result itself. Refreshing the branch buys nothing, pushes a "
        "new head, and cancels and restarts the required gate. Leave the head as it is and "
        f"land it with `shipyard ship --pr {facts.get('pr', '<n>')}`; refresh only if it "
        "conflicts or a required check fails"
    )


# ---------------------------------------------------------------------------
# Live reads through the real gh binary and the App token ghapp exported.
# ---------------------------------------------------------------------------


def _run_real_gh_text(arguments: list[str]) -> tuple[int, str, str]:
    real_gh = os.environ.get("GHAPP_REAL_GH", "/opt/homebrew/bin/gh")
    try:
        completed = subprocess.run(
            [real_gh, *arguments],
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise GuardError(f"cannot read live state: {error}") from error
    return completed.returncode, completed.stdout, completed.stderr


def run_real_gh(arguments: list[str]) -> Any:
    code, stdout, stderr = _run_real_gh_text(arguments)
    if code != 0:
        detail = stderr.strip().splitlines()
        raise GuardError("cannot read live state: " + (detail[-1] if detail else f"exit {code}"))
    try:
        return json.loads(stdout)
    except json.JSONDecodeError as error:
        raise GuardError(f"live state was not JSON: {error}") from error


def read_policy(owner: str, name: str, base: str) -> tuple[str, str | None]:
    """Read ``[merge] refresh_branch`` from the base branch's Shipyard config."""
    code, stdout, stderr = _run_real_gh_text(
        [
            "api",
            "-H",
            "Accept: application/vnd.github.raw",
            f"repos/{owner}/{name}/contents/{CONFIG_PATH}?ref={base}",
        ]
    )
    if code != 0:
        if "404" in stderr or "Not Found" in stderr:
            return POLICY_ALWAYS, None
        detail = stderr.strip().splitlines()
        raise GuardError(
            f"cannot read {CONFIG_PATH} on `{base}`: " + (detail[-1] if detail else f"exit {code}")
        )
    return normalize_policy(policy_from_toml(stdout))


def pull_request_state(owner: str, name: str, number: int, base: str) -> Any:
    """Read the PR, re-reading while GitHub has not computed mergeability yet."""
    arguments = [
        "api", "graphql", "-f", f"query={PR_QUERY}",
        "-f", f"owner={owner}", "-f", f"name={name}", "-F", f"number={number}",
        "-f", f"base={base}",
    ]
    response = run_real_gh(arguments)
    for _ in range(UNKNOWN_MERGEABLE_RETRIES):
        pr = _pull_request(response)
        if pr is None or pr.get("mergeable") != "UNKNOWN":
            break
        time.sleep(UNKNOWN_MERGEABLE_DELAY_SECONDS)
        response = run_real_gh(arguments)
    return response


_BASE_CACHE: dict[tuple[str, str, int], str] = {}


def base_branch(owner: str, name: str, number: int) -> str:
    key = (owner, name, number)
    if key not in _BASE_CACHE:
        value = run_real_gh(["api", f"repos/{owner}/{name}/pulls/{number}"])
        base = value.get("base", {}).get("ref") if isinstance(value, dict) else None
        if not isinstance(base, str) or not base:
            raise GuardError(f"cannot read PR #{number}'s base branch")
        _BASE_CACHE[key] = base
    return _BASE_CACHE[key]


# ---------------------------------------------------------------------------
# Request detection.
# ---------------------------------------------------------------------------

UPDATE_BRANCH_VALUE_OPTIONS = {"-R", "--repo"}
REST_UPDATE_BRANCH = re.compile(r"^repos/([^/]+)/([^/]+)/pulls/([1-9][0-9]*)/update-branch$")
GRAPHQL_REFRESH = re.compile(r"updatePullRequestBranch\s*\(", re.IGNORECASE)


def split_repo(repo: str) -> tuple[str, str]:
    parts = repo.strip().split("/")
    if len(parts) >= 2 and parts[-2] and parts[-1]:
        return parts[-2], parts[-1]
    raise GuardError(f"cannot resolve repository `{repo}`")


def _current_repo() -> tuple[str, str]:
    repo = os.environ.get("GH_REPO", "")
    if repo:
        return split_repo(repo)
    if PARSER is None:
        raise GuardError("cannot resolve the current repository")
    return PARSER.current_repo_identity()


def update_branch_target(args: list[str]) -> tuple[str, str, int]:
    """Resolve ``pr update-branch`` arguments to ``(owner, name, number)``."""
    selector = None
    repo = None
    skip = False
    for index, arg in enumerate(args[2:], start=2):
        if skip:
            skip = False
            continue
        if arg in UPDATE_BRANCH_VALUE_OPTIONS:
            skip = True
            if index + 1 < len(args):
                repo = args[index + 1]
            continue
        if arg.startswith("--repo="):
            repo = arg.removeprefix("--repo=")
            continue
        if arg.startswith("-"):
            continue
        if selector is None:
            selector = arg
    if selector:
        match = re.search(r"github\.com/([^/]+)/([^/]+)/pull/([1-9][0-9]*)", selector)
        if match:
            return match.group(1), match.group(2), int(match.group(3))
    owner, name = split_repo(repo) if repo else _current_repo()
    if selector and re.fullmatch(r"#?[1-9][0-9]*", selector):
        return owner, name, int(selector.lstrip("#"))
    view = ["pr", "view", "--repo", f"{owner}/{name}", "--json", "number"]
    if selector:
        view.insert(2, selector)
    value = run_real_gh(view)
    number = value.get("number") if isinstance(value, dict) else None
    if not isinstance(number, int):
        raise GuardError(f"cannot resolve the pull request for `{selector or 'current branch'}`")
    return owner, name, number


def graphql_refresh_targets(args: list[str], document: str) -> list[tuple[str, str, int]]:
    variables: dict[str, Any] = {}
    assert PARSER is not None
    for value in PARSER.option_values(args, {"-f", "-F", "--field", "--raw-field"}):
        key, separator, raw = value.partition("=")
        if separator and key != "query":
            variables[key] = raw
    for path in PARSER.option_values(args, {"--input"}):
        if path == "-":
            raise GuardError("cannot inspect a GraphQL body read from stdin")
        try:
            body = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise GuardError(f"cannot inspect GraphQL input: {error}") from error
        if isinstance(body, dict) and isinstance(body.get("variables"), dict):
            variables.update(body["variables"])
    targets = []
    for match in GRAPHQL_REFRESH.finditer(document):
        tail = document[match.end():]
        target = re.search(r"pullRequestId\s*:\s*(\$[A-Za-z_][A-Za-z0-9_]*|\"([^\"]+)\")", tail)
        if target is None:
            raise GuardError("cannot find the pullRequestId a branch refresh targets")
        node = target.group(2) if target.group(2) is not None else variables.get(target.group(1)[1:])
        if not isinstance(node, str) or not node:
            raise GuardError("cannot resolve the pullRequestId a branch refresh targets")
        value = run_real_gh(["api", "graphql", "-f", f"query={PR_ID_QUERY}", "-f", f"id={node}"])
        try:
            pr = value["data"]["node"]
            owner, name = split_repo(pr["repository"]["nameWithOwner"])
            targets.append((owner, name, int(pr["number"])))
        except (KeyError, TypeError, ValueError) as error:
            raise GuardError("cannot resolve the pull request a branch refresh targets") from error
    return targets


def refresh_request(args: list[str]) -> list[tuple[str, str, int]] | None:
    """``None`` when ``args`` cannot refresh a branch; else the targeted PRs."""
    if "--help" in args or "-h" in args:
        return None
    if len(args) >= 2 and args[0] == "pr" and args[1] == "update-branch":
        return [update_branch_target(args)]
    if not args or args[0] != "api" or PARSER is None:
        return None
    try:
        endpoint, query = PARSER.api_target(args)
    except PARSER.GuardError:
        return None
    rest = REST_UPDATE_BRANCH.match(endpoint)
    if rest:
        return [(rest.group(1), rest.group(2), int(rest.group(3)))]
    if endpoint != "graphql":
        return None
    try:
        document = "\n".join([PARSER.graphql_document(args), *query.get("query", [])])
    except PARSER.GuardError as error:
        raise GuardError(str(error)) from error
    if not GRAPHQL_REFRESH.search(document):
        return None
    return graphql_refresh_targets(args, document)


def _note(message: str) -> None:
    print(f"branch-refresh-guard: {message}", file=sys.stderr)


def main(args: list[str]) -> int:
    try:
        targets = refresh_request(args)
    except GuardError as error:
        _note(f"cannot inspect this branch refresh ({error}); allowing it")
        return 0
    if not targets:
        return 0
    override = os.environ.get("GHAPP_ALLOW_BRANCH_REFRESH") == "1"
    refusals = []
    for owner, name, number in targets:
        try:
            base = base_branch(owner, name, number)
            policy, warning = read_policy(owner, name, base)
            if warning:
                _note(warning)
            if policy == POLICY_ALWAYS:
                continue
            facts = refresh_facts(pull_request_state(owner, name, number, base))
        except GuardError as error:
            _note(f"{error}; allowing the refresh of PR #{number}")
            continue
        allowed, reason = decide(policy, facts)
        if not allowed:
            refusals.append(reason)
    if not refusals:
        return 0
    message = " | ".join(refusals)
    if override:
        _note("WARNING: GHAPP_ALLOW_BRANCH_REFRESH=1 overrides a refusal: " + message)
        return 0
    _note(f"refusing: {message}.")
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
